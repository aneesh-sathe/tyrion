use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommissionStatus {
    Proposed,
    Active,
    VerifiedComplete,
}

impl CommissionStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::VerifiedComplete => "verified_complete",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CriterionStatus {
    Pending,
    Passed,
    Failed,
}

impl CriterionStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AssignmentStatus {
    Ready,
    Running,
    Accepted,
    VerificationFailed,
    ResourceBlocked,
}

impl AssignmentStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Accepted => "accepted",
            Self::VerificationFailed => "verification_failed",
            Self::ResourceBlocked => "resource_blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptStatus {
    Running,
    Succeeded,
    Failed,
}

impl AttemptStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResultStatus {
    Candidate,
    Accepted,
}

impl ResultStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Accepted => "accepted",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceOutcome {
    Passed,
    Failed,
}

impl EvidenceOutcome {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }

    pub(crate) const fn criterion_status(self) -> CriterionStatus {
        match self {
            Self::Passed => CriterionStatus::Passed,
            Self::Failed => CriterionStatus::Failed,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EventKind {
    CommissionProposed,
    CommissionAccepted,
    AssignmentReady,
    AttemptStarted,
    ResultSubmitted,
    ResultAccepted,
    ResultIntegrated,
    EvidenceRecorded,
    CommissionVerifiedComplete,
    AssignmentBlocked,
    AttachmentJoined,
    ActiveAttachmentChanged,
}

impl EventKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CommissionProposed => "commission_proposed",
            Self::CommissionAccepted => "commission_accepted",
            Self::AssignmentReady => "assignment_ready",
            Self::AttemptStarted => "attempt_started",
            Self::ResultSubmitted => "result_submitted",
            Self::ResultAccepted => "result_accepted",
            Self::ResultIntegrated => "result_integrated",
            Self::EvidenceRecorded => "evidence_recorded",
            Self::CommissionVerifiedComplete => "commission_verified_complete",
            Self::AssignmentBlocked => "assignment_blocked",
            Self::AttachmentJoined => "attachment_joined",
            Self::ActiveAttachmentChanged => "active_attachment_changed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkerLeaseStatus {
    Active,
    Released,
    Revoked,
    Expired,
}

impl WorkerLeaseStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Released => "released",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthorityScopeType {
    Repository,
    Path,
    Action,
    Destination,
    Effect,
}

impl AuthorityScopeType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Path => "path",
            Self::Action => "action",
            Self::Destination => "destination",
            Self::Effect => "effect",
        }
    }
}
