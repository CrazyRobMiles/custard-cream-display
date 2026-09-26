//! The custard cream server's picture API (see `pages/random.js` and
//! `pages/pictures.js` in custard-cream-server). None of it needs a login.
//!
//! | Request                              | Reply                                  |
//! |--------------------------------------|----------------------------------------|
//! | `GET /random/tags`                   | `{"tags": ["emf2026", ...]}`           |
//! | `GET /random/batch?count=N&tags=a,b` | `{"pictures": [{"phrase": ...}, ...]}` |
//! | `GET /pictures/<phrase>/thumb?size=S`| JPEG, fitted inside S x S (480..1600)  |
//!
//! `batch` picks at random from pictures carrying *any* of the tags, or from
//! every picture if no tags are given.

use alloc::string::String;
use alloc::vec::Vec;

use embassy_net::Stack;
use serde::Deserialize;

use crate::config::server::{BASE_PATH, HOST, PORT};
use crate::http::{self, Error};

/// Pictures to ask for at a time (the server allows up to 20).
pub const BATCH_SIZE: usize = 10;

const MAX_JSON_BYTES: usize = 64 * 1024;
const MAX_JPEG_BYTES: usize = 1024 * 1024;

/// The server clamps `size` to this range.
const MIN_THUMB_SIZE: usize = 480;
const MAX_THUMB_SIZE: usize = 1600;

#[derive(Deserialize)]
struct TagList {
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct Batch {
    pictures: Vec<PictureRef>,
}

#[derive(Deserialize)]
struct PictureRef {
    phrase: String,
}

/// Every tag in use on the server, sorted.
pub async fn tags(stack: Stack<'_>) -> Result<Vec<String>, Error> {
    let path = path("random/tags");
    let body = http::get(stack, HOST, PORT, &path, MAX_JSON_BYTES).await?;
    let list: TagList = serde_json::from_slice(&body).map_err(|_| Error::BadReply)?;
    Ok(list.tags)
}

/// The phrases of up to [`BATCH_SIZE`] random pictures carrying any of
/// `tags`, or any pictures at all if `tags` is empty.
pub async fn random_batch(stack: Stack<'_>, tags: &[String]) -> Result<Vec<String>, Error> {
    let mut path = path("random/batch");
    path.push_str(&alloc::format!("?count={BATCH_SIZE}"));
    if !tags.is_empty() {
        path.push_str("&tags=");
        for (i, tag) in tags.iter().enumerate() {
            if i > 0 {
                path.push(',');
            }
            http::encode(tag, &mut path);
        }
    }
    let body = http::get(stack, HOST, PORT, &path, MAX_JSON_BYTES).await?;
    let batch: Batch = serde_json::from_slice(&body).map_err(|_| Error::BadReply)?;
    Ok(batch.pictures.into_iter().map(|p| p.phrase).collect())
}

/// A JPEG of the picture, scaled by the server so that it fits inside a
/// `size` x `size` square.
pub async fn picture(stack: Stack<'_>, phrase: &str, size: usize) -> Result<Vec<u8>, Error> {
    let mut path = path("pictures/");
    http::encode(phrase, &mut path);
    let size = size.clamp(MIN_THUMB_SIZE, MAX_THUMB_SIZE);
    path.push_str(&alloc::format!("/thumb?size={size}"));
    http::get(stack, HOST, PORT, &path, MAX_JPEG_BYTES).await
}

fn path(rest: &str) -> String {
    let mut path = String::from(BASE_PATH);
    path.push_str(rest);
    path
}
