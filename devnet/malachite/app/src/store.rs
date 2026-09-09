//! Atomic durable value + certificate records. Retain all local PoC history.
use color_eyre::eyre::{self, eyre};
use malachitebft_app_channel::app::types::core::CommitCertificate;
use malachitebft_eth_types::{codec::proto, Height, TestContext, Value};
use malachitebft_proto::Protobuf;
use prost::Message;
use redb::{Database, ReadableTable, TableDefinition};
use std::{path::Path, sync::Arc};

const VALUES: TableDefinition<u64, &[u8]> = TableDefinition::new("values");
const CERTIFICATES: TableDefinition<u64, &[u8]> = TableDefinition::new("certificates");

pub struct DecidedValue {
    pub value: Value,
    pub certificate: CommitCertificate<TestContext>,
}

pub struct Store(Arc<Database>);
impl Store {
    pub fn open(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let db = Database::create(path)?;
        let tx = db.begin_write()?;
        tx.open_table(VALUES)?;
        tx.open_table(CERTIFICATES)?;
        tx.commit()?;
        Ok(Self(Arc::new(db)))
    }

    pub async fn store_decided_value(
        &self,
        certificate: &CommitCertificate<TestContext>,
        value: Value,
    ) -> eyre::Result<()> {
        let height = certificate.height.as_u64();
        let certificate = proto::encode_certificate(certificate)?.encode_to_vec();
        let value = value.to_bytes()?;
        let db = self.0.clone();
        tokio::task::spawn_blocking(move || -> eyre::Result<()> {
            let tx = db.begin_write()?;
            {
                let mut values = tx.open_table(VALUES)?;
                if let Some(previous) = values.get(height)? {
                    if previous.value() != value.as_ref() {
                        return Err(eyre!("conflicting durable decision"));
                    }
                }
                values.insert(height, value.as_ref())?;
                tx.open_table(CERTIFICATES)?
                    .insert(height, certificate.as_slice())?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?
    }

    pub async fn get_decided_value(&self, height: Height) -> eyre::Result<Option<DecidedValue>> {
        let db = self.0.clone();
        tokio::task::spawn_blocking(move || -> eyre::Result<_> {
            let tx = db.begin_read()?;
            let values = tx.open_table(VALUES)?;
            let Some(value) = values.get(height.as_u64())? else {
                return Ok(None);
            };
            let certificates = tx.open_table(CERTIFICATES)?;
            let certificate = certificates
                .get(height.as_u64())?
                .ok_or_else(|| eyre!("missing certificate"))?;
            let certificate =
                malachitebft_eth_types::proto::CommitCertificate::decode(certificate.value())?;
            Ok(Some(DecidedValue {
                value: Value::from_bytes(value.value())?,
                certificate: proto::decode_certificate(certificate)?,
            }))
        })
        .await?
    }

    pub async fn min_decided_value_height(&self) -> Option<Height> {
        let db = self.0.clone();
        tokio::task::spawn_blocking(move || -> eyre::Result<Option<Height>> {
            let tx = db.begin_read()?;
            let table = tx.open_table(VALUES)?;
            let first = table
                .first()?
                .map(|(height, _)| Height::new(height.value()));
            Ok(first)
        })
        .await
        .ok()?
        .ok()?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use malachitebft_app_channel::app::types::core::Round;

    #[tokio::test]
    async fn decision_survives_reopen_and_cannot_be_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.db");
        let value = Value::new(Bytes::from_static(b"payload"));
        let certificate = CommitCertificate::new(Height::new(1), Round::new(0), value.id(), vec![]);
        {
            let store = Store::open(&path).unwrap();
            store
                .store_decided_value(&certificate, value.clone())
                .await
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let saved = store
            .get_decided_value(Height::new(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.value, value);
        assert_eq!(saved.certificate.value_id, certificate.value_id);
        let other = Value::new(Bytes::from_static(b"conflicting payload"));
        assert!(store
            .store_decided_value(&certificate, other)
            .await
            .is_err());
        assert_eq!(
            store
                .get_decided_value(Height::new(1))
                .await
                .unwrap()
                .unwrap()
                .value,
            value
        );
    }
}
