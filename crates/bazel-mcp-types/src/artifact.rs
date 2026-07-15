use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    File,
    Directory,
    TestLog,
    Coverage,
    Remote,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub kind: ArtifactKind,
    pub uri: String,
    pub size_bytes: Option<u64>,
    pub locally_available: bool,
}
