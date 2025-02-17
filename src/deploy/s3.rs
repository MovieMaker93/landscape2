//! This module defines the functionality of the deploy CLI subcommand for the
//! AWS S3 provider.

use crate::S3Args;
use anyhow::{format_err, Context, Result};
use aws_sdk_s3::primitives::{ByteStream, DateTime};
use futures::stream::{self, StreamExt};
use mime_guess::mime;
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    time::Instant,
};
use tracing::{debug, info, instrument};
use walkdir::WalkDir;

/// File name of the index document.
const INDEX_DOCUMENT: &str = "index.html";

/// Prefix used in the logos objects keys in S3.
const LOGOS_PREFIX: &str = "logos/";

/// Number of files to upload concurrently.
const UPLOAD_FILES_CONCURRENCY: usize = 20;

/// Type alias to represent an object key.
type Key = String;

/// Deploy landscape website to AWS S3.
#[instrument(skip_all)]
pub(crate) async fn deploy(args: &S3Args) -> Result<()> {
    info!("deploying landscape website..");
    let start = Instant::now();

    // Check required environment variables
    check_env_vars()?;

    // Setup AWS S3 client
    let config = aws_config::load_from_env().await;
    let s3_client = aws_sdk_s3::Client::new(&config);

    // Get objects already deployed
    let deployed_objects = get_deployed_objects(&s3_client, &args.bucket).await?;

    // Upload landscape website files (except index document)
    upload_files(&s3_client, &args.bucket, &args.landscape_dir, &deployed_objects).await?;

    // Upload index document if all the other files were uploaded successfully
    upload_index_document(&s3_client, &args.bucket, &args.landscape_dir, &deployed_objects).await?;

    let duration = start.elapsed().as_secs_f64();
    info!("landscape website deployed! (took: {:.3}s)", duration);

    Ok(())
}

/// Check that the required environment variables have been provided.
#[instrument(skip_all, err)]
fn check_env_vars() -> Result<()> {
    let required_env_vars = ["AWS_REGION", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"];

    for var in required_env_vars {
        let result = env::var(var);
        if result.is_err() || result.expect("var to be set").is_empty() {
            return Err(format_err!("required environment variable {var} not provided"));
        }
    }

    Ok(())
}

/// Get objects already deployed, returning their key and the creation date of
/// the object.
#[instrument(skip_all, err)]
async fn get_deployed_objects(
    s3_client: &aws_sdk_s3::Client,
    bucket: &str,
) -> Result<HashMap<Key, DateTime>> {
    let mut deployed_objects = HashMap::new();

    // List all objects in the bucket provided, collecting their key and
    // creation timestamp
    let mut continuation_token = None;
    loop {
        let mut request = s3_client.list_objects_v2().bucket(bucket);
        if let Some(token) = continuation_token {
            request = request.continuation_token(token);
        }
        let output = request.send().await?;
        if let Some(objects) = output.contents {
            for object in objects {
                let Some(key) = object.key else { continue };
                let Some(created_at) = object.last_modified else {
                    continue;
                };
                deployed_objects.insert(key, created_at);
            }
        }
        if !output.is_truncated {
            break;
        }
        continuation_token = output.next_continuation_token;
    }

    Ok(deployed_objects)
}

/// Upload landscape website files to S3 bucket. Given that logos filenames are
/// based on their content, we don't need to upload again existing ones.
#[instrument(skip_all, err)]
async fn upload_files(
    s3_client: &aws_sdk_s3::Client,
    bucket: &str,
    landscape_dir: &PathBuf,
    deployed_objects: &HashMap<Key, DateTime>,
) -> Result<()> {
    // Upload files in the landscape directory to the bucket provided
    let results: Vec<Result<()>> = stream::iter(WalkDir::new(landscape_dir))
        .map(|entry| async {
            // Check if the entry is a regular file
            let entry = entry?;
            if !entry.file_type().is_file() {
                return Ok(());
            }

            // Prepare object key
            let file_name = entry.path();
            let key = file_name
                .display()
                .to_string()
                .trim_start_matches(landscape_dir.display().to_string().as_str())
                .trim_start_matches('/')
                .to_string();

            // We'll upload the index document at the end when all the other
            // files have been uploaded successfully
            if key == INDEX_DOCUMENT {
                return Ok(());
            }

            // Skip files that start with a dot
            if key.starts_with('.') {
                return Ok(());
            }

            // Skip objects that don't need to be uploaded again
            if deployed_objects.contains_key(&key) {
                // Skip already deployed logos (logos filenames are based on
                // their content, we don't need to upload again existing ones)
                if key.starts_with(LOGOS_PREFIX) {
                    return Ok(());
                }

                // Skip objects when the remote copy is up to date
                let local_ts = DateTime::from(fs::metadata(file_name)?.modified()?);
                let remote_ts = deployed_objects.get(&key).expect("object to exist");
                if remote_ts >= &local_ts {
                    return Ok(());
                }
            }

            // Prepare object's body and content type
            let body = ByteStream::from_path(file_name).await?;
            let content_type = mime_guess::from_path(&key)
                .first()
                .ok_or(format_err!("cannot detect content type of key: {})", &key))?;

            // Upload file
            s3_client
                .put_object()
                .bucket(bucket)
                .key(&key)
                .body(body)
                .content_type(content_type.essence_str())
                .send()
                .await
                .context(format_err!("error uploading file {}", key))?;

            debug!(?key, "file uploaded");
            Ok(())
        })
        .buffer_unordered(UPLOAD_FILES_CONCURRENCY)
        .collect()
        .await;

    // Process results
    let mut errors_found = false;
    let mut errors = String::new();
    for result in results {
        if let Err(err) = result {
            errors_found = true;
            errors.push_str(&format!("- {err:?}\n"));
        }
    }
    if errors_found {
        return Err(format_err!("{errors}"));
    }

    Ok(())
}

/// Upload landscape website index document to S3 bucket.
#[instrument(skip_all, err)]
async fn upload_index_document(
    s3_client: &aws_sdk_s3::Client,
    bucket: &str,
    landscape_dir: &Path,
    deployed_objects: &HashMap<Key, DateTime>,
) -> Result<()> {
    // Prepare object's key, body and content type
    let file_name = landscape_dir.join(INDEX_DOCUMENT);
    let key = INDEX_DOCUMENT.to_string();
    let body = ByteStream::from_path(&file_name).await?;
    let content_type = mime::TEXT_HTML.essence_str();

    // Check if the remote copy is up to date
    if deployed_objects.contains_key(&key) {
        let local_ts = DateTime::from(fs::metadata(&file_name)?.modified()?);
        let remote_ts = deployed_objects.get(&key).expect("object to exist");
        if remote_ts >= &local_ts {
            return Ok(());
        }
    }

    // Upload file
    s3_client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(body)
        .content_type(content_type)
        .send()
        .await
        .context("error uploading index document")?;

    debug!("index document uploaded");
    Ok(())
}
