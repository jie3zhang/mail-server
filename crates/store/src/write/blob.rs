/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use ahash::AHashSet;
use trc::AddContext;
use utils::{BLOB_HASH_LEN, BlobHash};

use crate::{
    BlobClass, BlobStore, Deserialize, IterateParams, Store, U32_LEN, U64_LEN, ValueKey,
    write::BatchBuilder,
};

use super::{BlobOp, Operation, ValueClass, ValueOp, key::DeserializeBigEndian, now};

#[derive(Debug, PartialEq, Eq)]
pub struct BlobQuota {
    pub bytes: usize,
    pub count: usize,
}

impl Store {
    pub async fn blob_exists(&self, hash: impl AsRef<BlobHash> + Sync + Send) -> trc::Result<bool> {
        self.get_value::<()>(ValueKey {
            account_id: 0,
            collection: 0,
            document_id: 0,
            class: ValueClass::Blob(BlobOp::Commit {
                hash: hash.as_ref().clone(),
            }),
        })
        .await
        .map(|v| v.is_some())
        .caused_by(trc::location!())
    }

    pub async fn blob_quota(&self, account_id: u32) -> trc::Result<BlobQuota> {
        let from_key = ValueKey {
            account_id,
            collection: 0,
            document_id: 0,
            class: ValueClass::Blob(BlobOp::Reserve {
                hash: BlobHash::default(),
                until: 0,
            }),
        };
        let to_key = ValueKey {
            account_id: account_id + 1,
            collection: 0,
            document_id: 0,
            class: ValueClass::Blob(BlobOp::Reserve {
                hash: BlobHash::default(),
                until: 0,
            }),
        };

        let now = now();
        let mut quota = BlobQuota { bytes: 0, count: 0 };

        self.iterate(
            IterateParams::new(from_key, to_key).ascending(),
            |key, value| {
                let until = key.deserialize_be_u64(key.len() - U64_LEN)?;
                if until > now && value.len() == U32_LEN {
                    let bytes = u32::deserialize(value)?;
                    if bytes > 0 {
                        quota.bytes += bytes as usize;
                        quota.count += 1;
                    }
                }
                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())?;

        Ok(quota)
    }

    pub async fn blob_has_access(
        &self,
        hash: impl AsRef<BlobHash> + Sync + Send,
        class: impl AsRef<BlobClass> + Sync + Send,
    ) -> trc::Result<bool> {
        let key = match class.as_ref() {
            BlobClass::Reserved {
                account_id,
                expires,
            } if *expires > now() => ValueKey {
                account_id: *account_id,
                collection: 0,
                document_id: 0,
                class: ValueClass::Blob(BlobOp::Reserve {
                    hash: hash.as_ref().clone(),
                    until: *expires,
                }),
            },
            BlobClass::Linked {
                account_id,
                collection,
                document_id,
            } => ValueKey {
                account_id: *account_id,
                collection: *collection,
                document_id: *document_id,
                class: ValueClass::Blob(BlobOp::Link {
                    hash: hash.as_ref().clone(),
                }),
            },
            _ => return Ok(false),
        };

        self.get_value::<()>(key).await.map(|v| v.is_some())
    }

    pub async fn purge_blobs(&self, blob_store: BlobStore) -> trc::Result<()> {
        // Remove expired temporary blobs
        let from_key = ValueKey {
            account_id: 0,
            collection: 0,
            document_id: 0,
            class: ValueClass::Blob(BlobOp::Reserve {
                until: 0,
                hash: BlobHash::default(),
            }),
        };
        let to_key = ValueKey {
            account_id: u32::MAX,
            collection: 0,
            document_id: 0,
            class: ValueClass::Blob(BlobOp::Reserve {
                until: 0,
                hash: BlobHash::default(),
            }),
        };
        let mut delete_keys = Vec::new();
        let mut active_hashes = AHashSet::new();
        let now = now();
        self.iterate(
            IterateParams::new(from_key, to_key).ascending().no_values(),
            |key, _| {
                let hash = BlobHash::try_from_hash_slice(
                    key.get(U32_LEN..U32_LEN + BLOB_HASH_LEN)
                        .ok_or_else(|| trc::Error::corrupted_key(key, None, trc::location!()))?,
                )
                .unwrap();
                let until = key.deserialize_be_u64(key.len() - U64_LEN)?;
                if until <= now {
                    delete_keys.push((key.deserialize_be_u32(0)?, BlobOp::Reserve { until, hash }));
                } else {
                    active_hashes.insert(hash);
                }
                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())?;

        // Validate linked blobs
        let from_key = ValueKey {
            account_id: 0,
            collection: 0,
            document_id: 0,
            class: ValueClass::Blob(BlobOp::Link {
                hash: BlobHash::default(),
            }),
        };
        let to_key = ValueKey {
            account_id: u32::MAX,
            collection: u8::MAX,
            document_id: u32::MAX,
            class: ValueClass::Blob(BlobOp::Link {
                hash: BlobHash::new_max(),
            }),
        };
        let mut last_hash = BlobHash::default();
        self.iterate(
            IterateParams::new(from_key, to_key).ascending().no_values(),
            |key, _| {
                let hash = BlobHash::try_from_hash_slice(
                    key.get(0..BLOB_HASH_LEN)
                        .ok_or_else(|| trc::Error::corrupted_key(key, None, trc::location!()))?,
                )
                .unwrap();
                let document_id = key.deserialize_be_u32(key.len() - U32_LEN)?;

                if document_id != u32::MAX {
                    if last_hash != hash {
                        last_hash = hash;
                    }
                } else if last_hash != hash && !active_hashes.contains(&hash) {
                    // Unlinked or expired blob, delete.
                    delete_keys.push((0, BlobOp::Commit { hash }));
                }

                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())?;

        // Delete expired or unlinked blobs
        for (_, op) in &delete_keys {
            if let BlobOp::Commit { hash } = op {
                blob_store
                    .delete_blob(hash.as_ref())
                    .await
                    .caused_by(trc::location!())?;
            }
        }

        // Delete hashes
        let mut batch = BatchBuilder::new();
        let mut last_account_id = u32::MAX;
        for (account_id, op) in delete_keys.into_iter() {
            if batch.is_large_batch() {
                last_account_id = u32::MAX;
                self.write(batch.build_all())
                    .await
                    .caused_by(trc::location!())?;
                batch = BatchBuilder::new();
            }
            if matches!(op, BlobOp::Reserve { .. }) && account_id != last_account_id {
                batch.with_account_id(account_id);
                last_account_id = account_id;
            }
            batch.any_op(Operation::Value {
                class: ValueClass::Blob(op),
                op: ValueOp::Clear,
            });
        }
        if !batch.is_empty() {
            self.write(batch.build_all())
                .await
                .caused_by(trc::location!())?;
        }

        Ok(())
    }

    pub async fn blob_hash_unlink_account(&self, account_id: u32) -> trc::Result<()> {
        // Validate linked blobs
        let from_key = ValueKey {
            account_id: 0,
            collection: 0,
            document_id: 0,
            class: ValueClass::Blob(BlobOp::Link {
                hash: BlobHash::default(),
            }),
        };
        let to_key = ValueKey {
            account_id: u32::MAX,
            collection: u8::MAX,
            document_id: u32::MAX,
            class: ValueClass::Blob(BlobOp::Link {
                hash: BlobHash::new_max(),
            }),
        };
        let mut delete_keys = Vec::new();
        self.iterate(
            IterateParams::new(from_key, to_key).ascending().no_values(),
            |key, _| {
                let document_id = key.deserialize_be_u32(key.len() - U32_LEN)?;

                if document_id != u32::MAX && key.deserialize_be_u32(BLOB_HASH_LEN)? == account_id {
                    delete_keys.push((
                        key[BLOB_HASH_LEN + U32_LEN],
                        document_id,
                        BlobOp::Link {
                            hash: BlobHash::try_from_hash_slice(
                                key.get(0..BLOB_HASH_LEN).ok_or_else(|| {
                                    trc::Error::corrupted_key(key, None, trc::location!())
                                })?,
                            )
                            .unwrap(),
                        },
                    ));
                }

                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())?;

        // Unlink blobs
        let mut batch = BatchBuilder::new();
        batch.with_account_id(account_id);
        let mut last_collection = u8::MAX;
        for (collection, document_id, op) in delete_keys.into_iter() {
            if batch.is_large_batch() {
                self.write(batch.build_all())
                    .await
                    .caused_by(trc::location!())?;
                batch = BatchBuilder::new();
                batch.with_account_id(account_id);
                last_collection = u8::MAX;
            }
            if collection != last_collection {
                batch.with_collection(collection);
                last_collection = collection;
            }
            batch.update_document(document_id);
            batch.any_op(Operation::Value {
                class: ValueClass::Blob(op),
                op: ValueOp::Clear,
            });
        }
        if !batch.is_empty() {
            self.write(batch.build_all())
                .await
                .caused_by(trc::location!())?;
        }

        Ok(())
    }
}
