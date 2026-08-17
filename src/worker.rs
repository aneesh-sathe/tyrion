use crate::artifact::ArtifactRevision;

pub const ACTION: &str = "deterministic.echo";

pub struct CandidateResult {
    pub output: String,
    pub artifact_revision: ArtifactRevision,
}

pub(crate) trait Worker {
    fn configuration(&self) -> &'static str;
    fn execute(&self, goal: &str) -> CandidateResult;
}

pub(crate) struct DeterministicLocalWorker;

impl Worker for DeterministicLocalWorker {
    fn configuration(&self) -> &'static str {
        "deterministic-local-v1"
    }

    fn execute(&self, goal: &str) -> CandidateResult {
        let output = goal.to_owned();
        let artifact_revision = ArtifactRevision::for_content(&output);
        CandidateResult {
            output,
            artifact_revision,
        }
    }
}

pub(crate) struct CorruptArtifactRevisionWorker;

impl Worker for CorruptArtifactRevisionWorker {
    fn configuration(&self) -> &'static str {
        "fault-corrupt-artifact-revision-v1"
    }

    fn execute(&self, goal: &str) -> CandidateResult {
        let mut candidate = DeterministicLocalWorker.execute(goal);
        candidate.artifact_revision = ArtifactRevision::from_claim("sha256:forged");
        candidate
    }
}
