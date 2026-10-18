//! Stable identifiers used across the protocol.
//!
//! IDs are UUIDs rendered as lowercase hyphenated strings so they can be
//! stored as SQLite `TEXT`, printed in logs, and compared cheaply.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_newtype {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                s.parse::<Uuid>().map(Self)
            }
        }
    };
}

id_newtype!(/// Identifies one installation of SyncEngine on a device.
DeviceId);
id_newtype!(/// Identifies a logical shared folder, independent of any local root path.
FolderId);
id_newtype!(/// Identifies one file or directory entry inside a shared folder.
EntryId);
id_newtype!(/// Identifies one journal change; stable across retries.
ChangeId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_strings() {
        let id = DeviceId::new();
        let text = id.to_string();
        let parsed: DeviceId = text.parse().expect("parse");
        assert_eq!(parsed, id);
        assert_eq!(text.len(), 36);
    }

    #[test]
    fn ids_serialize_as_plain_strings() {
        let id = FolderId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        let back: FolderId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
        assert!(json.starts_with('"') && json.ends_with('"'));
    }

    #[test]
    fn distinct_ids_are_distinct() {
        assert_ne!(EntryId::new(), EntryId::new());
        assert_ne!(ChangeId::new(), ChangeId::new());
    }
}
