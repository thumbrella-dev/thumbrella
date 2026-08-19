#!/usr/bin/env python3
"""
Prepare the Thumbrella server for a release.

Pre-flight checks:
  - Working tree clean in each affected repo
  - On 'main' branch
  - Up to date with origin/main

Then applies:
  - Changelog: inserts version header above ## Development, leaves summary placeholder
  - Version bumps across all crates, npm packages
  - npm README sync from template

After running, review changes with `git diff` and commit manually.
Use `--force` to skip pre-flight checks (e.g. experimental branches).

Usage:
    python scripts/prepare.py v1.4.0              # prepare everything
    python scripts/prepare.py v1.4.0 --force      # skip pre-flight checks
    python scripts/prepare.py v1.4.0 --dry-run    # show what would change
"""

import argparse
import datetime
import json
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CHANGELOG_PATH = REPO / "CHANGELOG-1.md"
NPM_RELEASE_DIR = REPO / "release" / "npmjs"

# Version-bearing files.
# Each entry: (scope, repo_path, file_rel, key_path, is_toml, extra_json_keys)
# extra_json_keys: optional list of additional JSON paths to set to the same
#   version (used for optionalDependencies in npm packages).
VERSION_FILES = [
    ("Cargo.toml", "workspace.package.version", True, None),

    # NPM release packages (in thumbrella repo)
    ("release/npmjs/packages/server/package.json", "version", False,
     ["optionalDependencies.@thumbrella/server-linux-x64-gnu",
      "optionalDependencies.@thumbrella/server-win32-x64-msvc"]),
    ("release/npmjs/packages/server-linux-x64-gnu/package.json", "version", False, None),
    ("release/npmjs/packages/server-win32-x64-msvc/package.json", "version", False, None),
]



# --- helpers -----------------------------------------------------------------

def run(cmd, cwd=None, check=True):
    """Run a command, return CompletedProcess.  Does NOT print output."""
    return subprocess.run(
        cmd, cwd=cwd, capture_output=True, text=True, check=check
    )


def read_file(path):
    return path.read_text()


def write_file(path, content):
    path.write_text(content)


def today_str():
    return datetime.date.today().strftime("%Y/%m/%d")


def ansi(text, code):
    """Wrap text in an ANSI color code if stdout is a TTY."""
    if sys.stdout.isatty():
        return f"\033[{code}m{text}\033[0m"
    return text


def bold(text):
    return ansi(text, "1")


def green(text):
    return ansi(text, "32")


def yellow(text):
    return ansi(text, "33")


def red(text):
    return ansi(text, "31")


# --- pre-flight checks -------------------------------------------------------

def check_clean_working_tree(repo):
    """Ensure no uncommitted changes."""
    result = run(["git", "status", "--porcelain"], cwd=repo, check=False)
    if result.stdout.strip():
        print(red(f"  FAIL: {repo.name} has uncommitted changes:"))
        for line in result.stdout.strip().split("\n")[:10]:
            print(f"    {line}")
        return False
    return True


def check_on_main(repo):
    """Ensure we're on the 'main' branch."""
    result = run(["git", "branch", "--show-current"], cwd=repo, check=False)
    branch = result.stdout.strip()
    if branch != "main":
        print(red(f"  FAIL: {repo.name} is on '{branch}', expected 'main'"))
        return False
    return True


def check_up_to_date(repo):
    """Ensure local main is up to date with origin/main."""
    # Fetch quietly to get latest remote state.
    run(["git", "fetch", "origin", "main"], cwd=repo, check=False)
    # Compare local vs remote.
    result = run(
        ["git", "rev-list", "--left-right", "--count", "main...origin/main"],
        cwd=repo, check=False,
    )
    behind = 0
    try:
        parts = result.stdout.strip().split()
        if len(parts) == 2:
            behind = int(parts[1])
    except (ValueError, IndexError):
        pass
    if behind > 0:
        print(red(f"  FAIL: {repo.name} is {behind} commit(s) behind origin/main"))
        return False
    return True


def run_preflight(force):
    """Run all pre-flight checks.  Returns True if passed."""
    if force:
        print(yellow("  Pre-flight checks SKIPPED (--force)"))
        return True

    print("=== Pre-flight checks ===")
    all_ok = True
    repo = REPO
    print(f"\n  {bold(repo.name)}:")
    ok = True
    ok &= check_clean_working_tree(repo)
    ok &= check_on_main(repo)
    ok &= check_up_to_date(repo)
    if not ok:
        print(red("\n  Pre-flight checks FAILED."))
        print("  Fix the issues above or use --force to skip checks.")
        return False
    print(green("\n  All pre-flight checks passed."))
    return True


# --- version bumping ---------------------------------------------------------

def bump_toml_version(content, key_path, new_version):
    """Update a version field in TOML content."""
    parts = key_path.split(".")
    *section_parts, key = parts
    target_section = ".".join(section_parts) if section_parts else None

    current_section = None
    lines = content.split("\n")
    modified = False

    result = []
    for line in lines:
        section_match = re.match(r"^\[([^\]]+)\]", line)
        if section_match:
            current_section = section_match.group(1)

        if (target_section is None or current_section == target_section):
            m = re.match(rf"^({re.escape(key)})\s*=\s*\"(.+?)\"", line)
            if m:
                result.append(f'{key} = "{new_version}"')
                modified = True
                continue

        result.append(line)

    return "\n".join(result), modified


def bump_json_value(data, key_path, new_version):
    """Walk key_path into a parsed JSON dict and set the value.
    Returns True if changed."""
    parts = key_path.split(".")
    current = data
    for part in parts[:-1]:
        if part not in current:
            return False
        current = current[part]
    old = current.get(parts[-1])
    if old == new_version:
        return False
    current[parts[-1]] = new_version
    return True


def bump_json_version(content, key_path, new_version, extra_keys=None):
    """Update version fields in JSON content.  Returns (new_content, changed)."""
    data = json.loads(content)
    changed = bump_json_value(data, key_path, new_version)

    if extra_keys:
        for ek in extra_keys:
            if bump_json_value(data, ek, new_version):
                changed = True

    return json.dumps(data, indent=2) + "\n", changed


def bump_version(repo_path, file_rel, key_path, is_toml, extra_keys, new_version):
    """Bump version in a single file.  Returns True if changed."""
    file_path = repo_path / file_rel
    if not file_path.exists():
        print(f"  SKIP (not found): {file_path}")
        return False

    content = read_file(file_path)
    if is_toml:
        new_content, changed = bump_toml_version(content, key_path, new_version)
    else:
        new_content, changed = bump_json_version(
            content, key_path, new_version, extra_keys,
        )

    if not changed:
        print(f"  UNCHANGED (already {new_version}?): {file_path}")
        return False

    write_file(file_path, new_content)
    extra_info = ""
    if extra_keys:
        extra_info = f"  (+ {len(extra_keys)} dep(s))"
    print(f"  UPDATED: {file_path}  ({key_path} -> {new_version}){extra_info}")
    return True


# --- changelog ---------------------------------------------------------------

def roll_changelog(new_version):
    """Move Development bullet items into a new version section below it.

    Keeps ## Development at the top (always empty for the next cycle),
    and inserts the new version header below it with the previous
    Development content."""

    content = read_file(CHANGELOG_PATH)
    date = today_str()

    # Find "## Development" followed by optional content up to the next "## ".
    m = re.search(
        r"(^## Development\s*\n)"       # group 1: the Development header line
        r"(.*?)"                         # group 2: bullets / empty lines
        r"(?=^## \S)",                   # lookahead: next version header
        content,
        re.MULTILINE | re.DOTALL,
    )
    if not m:
        # Fallback: Development at end of file with no following version.
        m = re.search(
            r"(^## Development\s*\n)(.*?)$",
            content,
            re.MULTILINE | re.DOTALL,
        )
    if not m:
        print(f"  SKIP: '## Development' header not found in {CHANGELOG_PATH}")
        return False

    dev_header = m.group(1)    # "## Development\n"
    bullets = m.group(2).strip()  # the bullet items (if any)
    insert_pos = m.end(2)      # right after the Development section content

    new_section = (
        f"{dev_header}\n"                                    # empty Development
        f"## {new_version[1:]} - {date}\n"                       # new version
        f"\n"
        f"[summary: describe the key changes in this release]\n"
        f"\n"
    )
    if bullets:
        new_section += f"{bullets}\n\n"

    new_content = content[:m.start()] + new_section + content[insert_pos:]
    write_file(CHANGELOG_PATH, new_content)
    print(f"  UPDATED: {CHANGELOG_PATH}")
    print(f"    Inserted: ## {new_version[1:]} - {date} below ## Development")
    print(f"    Edit the [summary] placeholder before committing.")
    return True


# --- readme sync -------------------------------------------------------------

def sync_npm_readme():
    """Sync the npm release README from the user-facing release README."""
    src = NPM_RELEASE_DIR / "README.release.md"
    dst = NPM_RELEASE_DIR / "README.md"
    if not src.exists():
        print(f"  SKIP: source not found: {src}")
        return False
    content = read_file(src)
    write_file(dst, content)
    print(f"  UPDATED: {dst}  (from README.release.md)")
    return True


# --- diff summary ------------------------------------------------------------

def show_diff_summary():
    """Print a git diff --stat for each repo in scope."""
    print(f"\n{bold('=== Changes to review ===')}")
    repo = REPO
    result = run(["git", "diff", "--stat"], cwd=repo, check=False)
    if result.stdout.strip():
        print(f"\n  {bold(repo.name)}:")
        for line in result.stdout.strip().split("\n"):
            print(f"    {line}")
    # Also show untracked files
    result2 = run(
        ["git", "ls-files", "--others", "--exclude-standard"],
        cwd=repo, check=False,
    )
    if result2.stdout.strip():
        print(f"    (untracked: {result2.stdout.strip()})")


# --- main --------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="Prepare Thumbrella repos for release"
    )
    parser.add_argument("version", help="version tag, e.g. v1.4.0")
    parser.add_argument("--force", action="store_true",
                        help="skip pre-flight checks")
    args = parser.parse_args()

    version = args.version

    if not version.startswith("v") or len(version) < 6 or not (version[1].isdigit() and version[-1].isdigit()):
        print(red("Version must be formatted with 'v' prefix, like 'v1.2.3'"))

    print(f"=== Preparing release {version} ===\n")

    if not run_preflight(args.force):
        sys.exit(1)

    print("\n--- Changelog ---")
    roll_changelog(version)

    print("\n--- Version bumps ---")
    for file_rel, key_path, is_toml, extra_keys in VERSION_FILES:
        bump_version(REPO, file_rel, key_path, is_toml, extra_keys, version)

    print("\n--- README sync ---")
    sync_npm_readme()

    show_diff_summary()

    pretty = version[1:]
    if pretty.endswith(".0"):
        pretty = pretty[:-2]
    print(f"\n{yellow('Review the changes above, then commit and tag in each repo:')}")
    print(f"  git add -A && git commit -m 'Release {pretty}' && git tag {version}")


if __name__ == "__main__":
    main()
