/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

// TODO(enableNewSmithyRuntime): Delete this file when cleaning up middleware

use aws_sig_auth::signer::SignableBody;
use aws_smithy_http::body::SdkBody;
use aws_smithy_http::byte_stream::{self, ByteStream};
use aws_smithy_http::operation::Request;

use bytes::Buf;
use bytes_utils::SegmentedBuf;
use http::header::HeaderName;
use ring::digest::{Context, Digest, SHA256};
use tokio_stream::StreamExt;

const TREE_HASH_HEADER: &str = "x-amz-sha256-tree-hash";
const X_AMZ_CONTENT_SHA256: &str = "x-amz-content-sha256";

/// Adds a glacier tree hash checksum to the HTTP Request
///
/// This handles two cases:
/// 1. A body which is retryable: the body will be streamed through a digest calculator, limiting memory usage.
/// 2. A body which is not retryable: the body will be converted into `Bytes`, then streamed through a digest calculator.
///
/// The actual checksum algorithm will first compute a SHA256 checksum for each 1MB chunk. Then, a tree
/// will be assembled, recursively pairing neighboring chunks and computing their combined checksum. The 1 leftover
/// chunk (if it exists) is paired at the end.
///
/// See <https://docs.aws.amazon.com/amazonglacier/latest/dev/checksum-calculations.html> for more information.
pub async fn add_checksum_treehash(request: &mut Request) -> Result<(), byte_stream::error::Error> {
    let cloneable = request.http().body().try_clone();
    let http_request = request.http_mut();
    let body_to_process = if let Some(cloned_body) = cloneable {
        // we can stream the body
        cloned_body
    } else {
        let body = std::mem::replace(http_request.body_mut(), SdkBody::taken());
        let loaded_body = ByteStream::new(body).collect().await?.into_bytes();
        *http_request.body_mut() = SdkBody::from(loaded_body.clone());
        SdkBody::from(loaded_body)
    };
    let (full_body, hashes) = compute_hashes(body_to_process, MEGABYTE).await?;
    let tree_hash = hex::encode(compute_hash_tree(hashes));
    let complete_hash = hex::encode(full_body);
    if !http_request.headers().contains_key(TREE_HASH_HEADER) {
        http_request.headers_mut().insert(
            HeaderName::from_static(TREE_HASH_HEADER),
            tree_hash.parse().expect("hash must be valid header"),
        );
    }
    if !http_request.headers().contains_key(X_AMZ_CONTENT_SHA256) {
        http_request.headers_mut().insert(
            HeaderName::from_static(X_AMZ_CONTENT_SHA256),
            complete_hash.parse().expect("hash must be valid header"),
        );
    }
    // if we end up hitting the signer later, no need to recompute the checksum
    request
        .properties_mut()
        .insert(SignableBody::Precomputed(complete_hash));
    // for convenience & protocol tests, write it in directly here as well
    Ok(())
}

const MEGABYTE: usize = 1024 * 1024;
async fn compute_hashes(
    body: SdkBody,
    chunk_size: usize,
) -> Result<(Digest, Vec<Digest>), byte_stream::error::Error> {
    let mut hashes = vec![];
    let mut remaining_in_chunk = chunk_size;
    let mut body = ByteStream::new(body);
    let mut local = Context::new(&SHA256);
    let mut full_body = Context::new(&SHA256);
    let mut segmented = SegmentedBuf::new();
    while let Some(data) = body.try_next().await? {
        segmented.push(data);
        while segmented.has_remaining() {
            let next = segmented.chunk();
            let len = next.len().min(remaining_in_chunk);
            local.update(&next[..len]);
            full_body.update(&next[..len]);
            segmented.advance(len);
            remaining_in_chunk -= len;
            if remaining_in_chunk == 0 {
                hashes.push(local.finish());
                local = Context::new(&SHA256);
                remaining_in_chunk = chunk_size;
            }
        }
    }
    if remaining_in_chunk != chunk_size || hashes.is_empty() {
        hashes.push(local.finish());
    }
    Ok((full_body.finish(), hashes))
}

/// Compute the glacier tree hash for a vector of hashes.
///
/// Adjacent hashes are combined into a single hash. This process occurs recursively until only 1 hash remains.
///
/// See <https://docs.aws.amazon.com/amazonglacier/latest/dev/checksum-calculations.html> for more information.
fn compute_hash_tree(mut hashes: Vec<Digest>) -> Digest {
    assert!(
        !hashes.is_empty(),
        "even an empty file will produce a digest. this function assumes that hashes is non-empty"
    );
    while hashes.len() > 1 {
        let next = hashes.chunks(2).into_iter().map(|chunk| match *chunk {
            [left, right] => {
                let mut ctx = Context::new(&SHA256);
                ctx.update(left.as_ref());
                ctx.update(right.as_ref());
                ctx.finish()
            }
            [last] => last,
            _ => unreachable!(),
        });
        hashes = next.collect();
    }
    hashes[0]
}

#[cfg(test)]
mod test {
    use crate::glacier_checksums::{
        add_checksum_treehash, compute_hash_tree, compute_hashes, MEGABYTE, TREE_HASH_HEADER,
    };
    use aws_smithy_http::body::SdkBody;
    use aws_smithy_http::byte_stream::ByteStream;
    use aws_smithy_http::operation::Request;

    #[tokio::test]
    async fn compute_digests() {
        {
            let body = SdkBody::from("1234");
            let hashes = compute_hashes(body, 1).await.expect("succeeds").1;
            assert_eq!(hashes.len(), 4);
        }
        {
            let body = SdkBody::from("1234");
            let hashes = compute_hashes(body, 2).await.expect("succeeds").1;
            assert_eq!(hashes.len(), 2);
        }
        {
            let body = SdkBody::from("12345");
            let hashes = compute_hashes(body, 3).await.expect("succeeds").1;
            assert_eq!(hashes.len(), 2);
        }
        {
            let body = SdkBody::from("11221122");
            let hashes = compute_hashes(body, 2).await.expect("succeeds").1;
            assert_eq!(hashes[0].as_ref(), hashes[2].as_ref());
        }
    }

    #[tokio::test]
    async fn empty_body_computes_digest() {
        let body = SdkBody::from("");
        let (_, hashes) = compute_hashes(body, 2).await.expect("succeeds");
        assert_eq!(hashes.len(), 1);
    }

    #[tokio::test]
    async fn compute_tree_digest() {
        macro_rules! hash {
            ($($inp:expr),*) => {
                {
                    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
                    $(
                        ctx.update($inp.as_ref());
                    )*
                    ctx.finish()
                }
            }
        }
        let body = SdkBody::from("1234567891011");
        let (complete, hashes) = compute_hashes(body, 3).await.expect("succeeds");
        assert_eq!(hashes.len(), 5);
        assert_eq!(complete.as_ref(), hash!("1234567891011").as_ref());
        let final_digest = compute_hash_tree(hashes);
        let expected_digest = hash!(
            hash!(
                hash!(hash!("123"), hash!("456")),
                hash!(hash!("789"), hash!("101"))
            ),
            hash!("1")
        );
        assert_eq!(expected_digest.as_ref(), final_digest.as_ref());
    }

    #[tokio::test]
    async fn integration_test() {
        // the test data consists of an 11 byte sequence, repeated. Since the sequence length is
        // relatively prime with 1 megabyte, we can ensure that chunks will all have different hashes.
        let base_seq = b"01245678912";
        let total_size = MEGABYTE * 101 + 500;
        let mut test_data = vec![];
        while test_data.len() < total_size {
            test_data.extend_from_slice(base_seq)
        }
        let target = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(target.path(), test_data).await.unwrap();
        let body = ByteStream::from_path(target.path())
            .await
            .expect("should be valid")
            .into_inner();

        let mut http_req = Request::new(
            http::Request::builder()
                .uri("http://example.com/hello")
                .body(body)
                .unwrap(),
        );

        add_checksum_treehash(&mut http_req)
            .await
            .expect("should succeed");
        // hash value verified with AWS CLI
        assert_eq!(
            http_req.http().headers().get(TREE_HASH_HEADER).unwrap(),
            "3d417484359fc9f5a3bafd576dc47b8b2de2bf2d4fdac5aa2aff768f2210d386"
        );
    }
}
