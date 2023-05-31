// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::Chunk;
use crate::error::Error as ProtocolError;
use libp2p::kad::{Record, RecordKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct RecordHeader {
    pub kind: RecordKind,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RecordKind {
    Chunk,
    DbcSpend,
    Register,
}

impl RecordHeader {
    // Bincode serializes enums with unit variants as a u32, not a u8, so it would take up 4 bytes.
    pub const SIZE: usize = 4;

    pub fn deserialize(bytes: &[u8]) -> Result<Self, ProtocolError> {
        bincode::deserialize(bytes).map_err(|_| ProtocolError::FailedToDeserializeRecordHeader)
    }

    pub fn serialize(&self) -> Result<Vec<u8>, ProtocolError> {
        bincode::serialize(&self).map_err(|_| ProtocolError::FailedToDeserializeRecordHeader)
    }
}

impl TryFrom<Chunk> for Record {
    type Error = ProtocolError;

    fn try_from(value: Chunk) -> Result<Self, Self::Error> {
        let mut record_value = RecordHeader {
            kind: RecordKind::Chunk,
        }
        .serialize()?;
        record_value.extend_from_slice(value.value());

        Ok(Record {
            key: RecordKey::new(value.address().name()),
            value: record_value,
            publisher: None,
            expires: None,
        })
    }
}

impl TryFrom<Record> for Chunk {
    type Error = ProtocolError;

    fn try_from(value: Record) -> Result<Self, Self::Error> {
        let header = RecordHeader::deserialize(&value.value)?;
        if !matches!(header.kind, RecordKind::Chunk) {
            //todo: impl correct error type
            return Err(ProtocolError::FailedToDeserializeRecordHeader);
        }

        let value = value.value[RecordHeader::SIZE..].to_vec();
        Ok(Chunk::new(value.into()))
    }
}
