
## Schema for api calls

Subject to change dramatically, more research will create new requirements
and new restrictions and new use cases.


Regular Thumbnail Request
    Api required in header (when coming through my cloudflare service)
    Each media (payload):
        url: media to request
        etag: optional string from previous lookup
    (some flag to determine format? streaming json, sse, single lump result?)


Regular Thumbnail Response
    Individual url results are streamed in arbitrary order
    If client drops connection work is immediately cancelled

    Status:
        request id: (also includes identification for which server ran this)
        status: ok / bad-credential / bad-request / server-misconfigure / server-maintenance / ...
        message: optional string
    for each incoming url (one response for every url)
        url: original url requested
        job_duration: seconds to generate
        job_data: bytes read to generate
        job_status: success / cached / not_modified / failed / defer
          (note, on anything other than 'success' no following fields provided)
            (perhaps there is a "message" for those cases?)
          (defer may need different between user-throttled and server-throttled?)
        job_strategy: render / progressive / embedded / fallback
        etag: optional string for followup lookup
        thumbnail: base64 binary (always a 250x200 low quality jpeg3)
        mime_type: traditional mime type
        file_length: known content length (if known, otherwise null?)
        media_type: image / video / audio / vector / document / geometry / text / binary / archive
        file_extension: jpg / png / pdf / exr / md / zip (this is an enumerated type, not the original media filename, it will not include variants like "jpeg" or "htm")
        properties; (dependent on file_type, not everything has properites but we aim for a generic consistent set of fields for ones that do)
            ..image: (width, height, color depth)
            ..video: (width, height, color depth, duration)
            ..vector: (aspect ratio, ?)
            ..audio: (duration, channels)
            ..text: (lines, encoding?)  (actually, lines is hard to know without reading full contents, dubious)

(the mime types and extensions likely want coverage on https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/MIME_types/Common_types)


Direct Thumbnail Request
    Separate endpoint that directly returns the thumbnail jpg
    Media url is directly encoded in url path "http://thumbrella.dev/nail.jpg&url=http://myserver.com/logo.svg"
    Still allows Etag cache management in headers


Request Promotion from lower tier
    need some key/credential that we can trust this came from another thumbrella server
        and a version/compatibility flag?
    the etag should already have been checked so that can be skipped
    ideally the lower layer should also have done the initial file type identification
        so that should be sent and skipped
    possibly the request also comes with the first 1k (and optionally last 1K) of file data, if that has already been sniffed (actual sizes tbd?)
    this tier will do the final processing/jpeg generation
    but defer final caching back to the underlying layer


Internally we will want to keep a general database of every thumbnail created.
This may just be the "cache entries" so we don't need to duplicate some of 
the fields for that use case?

This has more internal details than we expose to clients.

Also note, the cached info almost 99% must be keyed by the user id, otherwise
people can leak media details from other people's s3 buckets (bad)


Media Logged
    internally every handled media will log the work that went into a request
    this may go to a time logged storage eventually, but for now just dump to
    a log file.

    This will essentially be the same as the regular thumbnail response, but
    exposes internal details and complications I'd rather not pollute the
    regular responses with? 
    (although this does not include the actual thumbnail or properties or etag)

    timestamp
    server id
    url 
        (i might even want to store this hashed so the internal urls are private even to me)
        (this must come from the actual resource url, not the expiring temporary access url)
    thumbnail_size
    job_data
    job_tail_data
    job_tier
    job_renderer
    job_strategy: render / progressive / embedded
    job_image_buffer_width
    job_image_buffer_height
    download_time
    process_time
    (other job notes from the process; codec, offset_time, stream, )
    file_length: known content length (if known, otherwise null?)
    file_type
    file_extension


    Then every request for a thumbnail is also logged, referencing that media cache entry
    logged_media_id
    timestamp
    user id
    server id
    session id (or something that lets me track which thumbnails came in a single request)
    http_agent
    job_status
    job_time

    failures are likely logged somehow also? seems like a separate entity, but
    I have no idea what I'd track at this point, still, something that says
    "something failed" while still attempting to keep the data somewhat anon?
