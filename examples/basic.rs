//! Thumbrella Rust client example
//!
//! This is an overly simplified example of using the Rust client for Thumbrella.
//! The actual Rust client lives in
//!
//!- **Crates** at https://crates.io/crates/thumbrella-client
//!- **Github** at https://github.com/thumbrella-dev/clients/
//!
//!See the more complete Rust client examples at
//!https://github.com/thumbrella-dev/clients/tree/main/rust/examples
//!

use std::fs;
use thumbrella_client::Client;


#[tokio::main]
async fn main() {

    // Client uses `$TBR_CONNECT` to define the server url or Cloud token
    let tbr = Client::new(None);


    // Generate a single specific thumbnail from url
    let url = "https://demo.thumbrella.dev/media/golden-gate.exr";
    let result = tbr.thumb(url).await?;
    let media = &result.media;
    println!("{} {} bytes -> {} bytes", 
        media.kind, media.file_size, media.thumbnail.len());


    // Write thumbnail to disk
    fs::write("/tmp/thumbnail.jpeg", media.thumbnail.bytes())?;

    ()
}

