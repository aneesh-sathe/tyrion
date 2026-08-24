use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommissionStatus {
    Proposed,
    Active,
    Paused,
    Cancelled,
    VerifiedComplete,
}

impl CommissionStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
            Self::VerifiedComplete => "verified_complete",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CriterionStatus {
    Uncertain,
    Passed,
    Failed,
}

impl CriterionStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Uncertain => "uncertain",
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
    Superseded,
    VerificationPending,
    VerificationFailed,
    ResourceBlocked,
    AttentionRequired,
    Cancelled,
}

impl AssignmentStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Accepted => "accepted",
            Self::Superseded => "superseded",
            Self::VerificationPending => "verification_pending",
            Self::VerificationFailed => "verification_failed",
            Self::ResourceBlocked => "resource_blocked",
            Self::AttentionRequired => "attention_required",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptStatus {
    Running,
    Succeeded,
    Failed,
    Interrupted,
    TimedOut,
    Cancelled,
}

impl AttemptStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResultStatus {
    Candidate,
    Accepted,
    Superseded,
}

impl ResultStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Accepted => "accepted",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceOutcome {
    Passed,
    Failed,
    Uncertain,
}

impl EvidenceOutcome {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }

    pub(crate) const fn criterion_status(self) -> CriterionStatus {
        match self {
            Self::Passed => CriterionStatus::Passed,
            Self::Failed => CriterionStatus::Failed,
            Self::Uncertain => CriterionStatus::Uncertain,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EventKind {
    CommissionProposed,
    CommissionAccepted,
    CommissionAmended,
    PlanRevised,
    AssignmentReady,
    AttemptStarted,
    ResourcesReserved,
    ResultSubmitted,
    ResultAccepted,
    ResultIntegrated,
    ReconciliationRequired,
    UsefulConcurrencyObserved,
    EvidenceRecorded,
    CommissionVerifiedComplete,
    AssignmentBlocked,
    AttachmentJoined,
    ActiveAttachmentChanged,
    WorkerSteered,
    WorkerInterrupted,
    WorkerActivity,
    CommissionPaused,
    CommissionResumed,
    CommissionCancelled,
    RecoveryDecided,
    AttemptContained,
    RestartReconciled,
}

impl EventKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CommissionProposed => "commission_proposed",
            Self::CommissionAccepted => "commission_accepted",
            Self::CommissionAmended => "commission_amended",
            Self::PlanRevised => "plan_revised",
            Self::AssignmentReady => "assignment_ready",
            Self::AttemptStarted => "attempt_started",
            Self::ResourcesReserved => "resources_reserved",
            Self::ResultSubmitted => "result_submitted",
            Self::ResultAccepted => "result_accepted",
            Self::ResultIntegrated => "result_integrated",
            Self::ReconciliationRequired => "reconciliation_required",
            Self::UsefulConcurrencyObserved => "useful_concurrency_observed",
            Self::EvidenceRecorded => "evidence_recorded",
            Self::CommissionVerifiedComplete => "commission_verified_complete",
            Self::AssignmentBlocked => "assignment_blocked",
            Self::AttachmentJoined => "attachment_joined",
            Self::ActiveAttachmentChanged => "active_attachment_changed",
            Self::WorkerSteered => "worker_steered",
            Self::WorkerInterrupted => "worker_interrupted",
            Self::WorkerActivity => "worker_activity",
            Self::CommissionPaused => "commission_paused",
            Self::CommissionResumed => "commission_resumed",
            Self::CommissionCancelled => "commission_cancelled",
            Self::RecoveryDecided => "recovery_decided",
            Self::AttemptContained => "attempt_contained",
            Self::RestartReconciled => "restart_reconciled",
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
