use super::{Blob, FileRange, StorageTransaction};
use crate::{Config, Metrics};
use chrono::{DateTime, NaiveDateTime, Utc};
use failure::Error;
use futures_util::{
    future::TryFutureExt,
    stream::{FuturesUnordered, StreamExt},
};
use rusoto_core::{region::Region, RusotoError};
use rusoto_credential::DefaultCredentialsProvider;
use rusoto_s3::{
    DeleteObjectsRequest, GetObjectError, GetObjectRequest, HeadObjectError, HeadObjectRequest,
    ListObjectsV2Request, ObjectIdentifier, PutObjectRequest, S3Client, S3,
};
use std::{convert::TryInto, io::Write, sync::Arc};
use tokio::runtime::Runtime;

pub(super) struct S3Backend {
    client: S3Client,
    runtime: Runtime,
    bucket: String,
    metrics: Arc<Metrics>,
    #[cfg(test)]
    temporary: bool,
}

impl S3Backend {
    pub(super) fn new(metrics: Arc<Metrics>, config: &Config) -> Result<Self, Error> {
        let runtime = Runtime::new()?;

        // Connect to S3
        let client = S3Client::new_with(
            rusoto_core::request::HttpClient::new()?,
            DefaultCredentialsProvider::new()?,
            config
                .s3_endpoint
                .as_deref()
                .map(|endpoint| Region::Custom {
                    name: config.s3_region.name().to_string(),
                    endpoint: endpoint.to_string(),
                })
                .unwrap_or_else(|| config.s3_region.clone()),
        );

        #[cfg(test)]
        {
            // Create the temporary S3 bucket during tests.
            if config.s3_bucket_is_temporary {
                if cfg!(not(test)) {
                    panic!("safeguard to prevent creating temporary buckets outside of tests");
                }

                runtime.block_on(client.create_bucket(rusoto_s3::CreateBucketRequest {
                    bucket: config.s3_bucket.clone(),
                    ..Default::default()
                }))?;
            }
        }

        Ok(Self {
            client,
            runtime,
            metrics,
            bucket: config.s3_bucket.clone(),
            #[cfg(test)]
            temporary: config.s3_bucket_is_temporary,
        })
    }

    pub(super) fn exists(&self, path: &str) -> Result<bool, Error> {
        self.runtime.block_on(async {
            let req = HeadObjectRequest {
                bucket: self.bucket.clone(),
                key: path.into(),
                ..Default::default()
            };
            let resp = self.client.head_object(req).await;
            match resp {
                Ok(_) => Ok(true),
                Err(RusotoError::Service(HeadObjectError::NoSuchKey(_))) => Ok(false),
                Err(RusotoError::Unknown(resp)) if resp.status == 404 => Ok(false),
                Err(other) => Err(other.into()),
            }
        })
    }

    pub(super) fn get(
        &self,
        path: &str,
        max_size: usize,
        range: Option<FileRange>,
    ) -> Result<Blob, Error> {
        self.runtime.block_on(async {
            let res = self
                .client
                .get_object(GetObjectRequest {
                    bucket: self.bucket.to_string(),
                    key: path.into(),
                    range: range.map(|r| format!("bytes={}-{}", r.start(), r.end())),
                    ..Default::default()
                })
                .await
                .map_err(|err| match err {
                    RusotoError::Service(GetObjectError::NoSuchKey(_)) => {
                        super::PathNotFoundError.into()
                    }
                    RusotoError::Unknown(http) if http.status == 404 => {
                        super::PathNotFoundError.into()
                    }
                    err => Error::from(err),
                })?;

            let mut content = crate::utils::sized_buffer::SizedBuffer::new(max_size);
            content.reserve(
                res.content_length
                    .and_then(|l| l.try_into().ok())
                    .unwrap_or(0),
            );

            let mut body = res
                .body
                .ok_or_else(|| failure::err_msg("Received a response from S3 with no body"))?;

            while let Some(data) = body.next().await.transpose()? {
                content.write_all(data.as_ref())?;
            }

            let date_updated = res
                .last_modified
                // This is a bug from AWS, it should always have a modified date of when it was created if nothing else.
                // Workaround it by passing now as the modification time, since the exact time doesn't really matter.
                .map_or(Ok(Utc::now()), |lm| parse_timespec(&lm))?;

            let compression = res.content_encoding.and_then(|s| s.parse().ok());

            Ok(Blob {
                path: path.into(),
                mime: res.content_type.unwrap(),
                date_updated,
                content: content.into_inner(),
                compression,
            })
        })
    }

    pub(super) fn start_storage_transaction(&self) -> S3StorageTransaction {
        S3StorageTransaction { s3: self }
    }

    #[cfg(test)]
    pub(super) fn cleanup_after_test(&self) -> Result<(), Error> {
        if !self.temporary {
            return Ok(());
        }

        if cfg!(not(test)) {
            panic!("safeguard to prevent deleting the production bucket");
        }

        let mut transaction = Box::new(self.start_storage_transaction());
        transaction.delete_prefix("")?;
        transaction.complete()?;

        self.runtime
            .block_on(self.client.delete_bucket(rusoto_s3::DeleteBucketRequest {
                bucket: self.bucket.clone(),
                ..Default::default()
            }))?;

        Ok(())
    }
}

pub(super) struct S3StorageTransaction<'a> {
    s3: &'a S3Backend,
}

impl<'a> StorageTransaction for S3StorageTransaction<'a> {
    fn store_batch(&mut self, mut batch: Vec<Blob>) -> Result<(), Error> {
        self.s3.runtime.block_on(async {
            // Attempt to upload the batch 3 times
            for _ in 0..3 {
                let mut futures = FuturesUnordered::new();
                for blob in batch.drain(..) {
                    futures.push(
                        self.s3
                            .client
                            .put_object(PutObjectRequest {
                                bucket: self.s3.bucket.to_string(),
                                key: blob.path.clone(),
                                body: Some(blob.content.clone().into()),
                                content_type: Some(blob.mime.clone()),
                                content_encoding: blob
                                    .compression
                                    .as_ref()
                                    .map(|alg| alg.to_string()),
                                ..Default::default()
                            })
                            .map_ok(|_| {
                                self.s3.metrics.uploaded_files_total.inc();
                            })
                            .map_err(|err| {
                                log::error!("Failed to upload blob to S3: {:?}", err);
                                // Reintroduce failed blobs for a retry
                                blob
                            }),
                    );
                }

                while let Some(result) = futures.next().await {
                    // Push each failed blob back into the batch
                    if let Err(blob) = result {
                        batch.push(blob);
                    }
                }

                // If we uploaded everything in the batch, we're done
                if batch.is_empty() {
                    return Ok(());
                }
            }

            panic!("failed to upload 3 times, exiting");
        })
    }

    fn delete_prefix(&mut self, prefix: &str) -> Result<(), Error> {
        self.s3.runtime.block_on(async {
            let mut continuation_token = None;
            loop {
                let list = self
                    .s3
                    .client
                    .list_objects_v2(ListObjectsV2Request {
                        bucket: self.s3.bucket.clone(),
                        prefix: Some(prefix.into()),
                        continuation_token,
                        ..ListObjectsV2Request::default()
                    })
                    .await?;

                let to_delete = list
                    .contents
                    .unwrap_or_else(Vec::new)
                    .into_iter()
                    .filter_map(|o| o.key)
                    .map(|key| ObjectIdentifier {
                        key,
                        version_id: None,
                    })
                    .collect::<Vec<_>>();

                let resp = self
                    .s3
                    .client
                    .delete_objects(DeleteObjectsRequest {
                        bucket: self.s3.bucket.clone(),
                        delete: rusoto_s3::Delete {
                            objects: to_delete,
                            quiet: None,
                        },
                        ..DeleteObjectsRequest::default()
                    })
                    .await?;

                if let Some(errs) = resp.errors {
                    for err in &errs {
                        log::error!("error deleting file from s3: {:?}", err);
                    }

                    failure::bail!("deleting from s3 failed");
                }

                continuation_token = list.next_continuation_token;
                if continuation_token.is_none() {
                    return Ok(());
                }
            }
        })
    }

    fn complete(self: Box<Self>) -> Result<(), Error> {
        Ok(())
    }
}

fn parse_timespec(mut raw: &str) -> Result<DateTime<Utc>, Error> {
    raw = raw.trim_end_matches(" GMT");

    Ok(DateTime::from_utc(
        NaiveDateTime::parse_from_str(raw, "%a, %d %b %Y %H:%M:%S")?,
        Utc,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_parse_timespec() {
        // Test valid conversions
        assert_eq!(
            parse_timespec("Thu, 1 Jan 1970 00:00:00 GMT").unwrap(),
            Utc.ymd(1970, 1, 1).and_hms(0, 0, 0),
        );
        assert_eq!(
            parse_timespec("Mon, 16 Apr 2018 04:33:50 GMT").unwrap(),
            Utc.ymd(2018, 4, 16).and_hms(4, 33, 50),
        );

        // Test invalid conversion
        assert!(parse_timespec("foo").is_err());
    }

    // The tests for this module are in src/storage/mod.rs, as part of the backend tests. Please
    // add any test checking the public interface there.

    // NOTE: trying to upload a file ending with `/` will behave differently in test and prod.
    // NOTE: On s3, it will succeed and create a file called `/`.
    // NOTE: On min.io, it will fail with 'Object name contains unsupported characters.'
}
