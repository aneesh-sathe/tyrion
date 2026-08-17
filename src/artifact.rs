use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactRevision(String);

impl ArtifactRevision {
    pub(crate) fn for_content(content: &str) -> Self {
        Self(format!("sha256:{:x}", Sha256::digest(content.as_bytes())))
    }

    pub(crate) fn from_claim(claim: impl Into<String>) -> Self {
        Self(claim.into())
    }

    pub(crate) fn matches_content(&self, content: &str) -> bool {
        self == &Self::for_content(content)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
