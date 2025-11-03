use crate::id::{JobId, RequestId, UserId};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn name(self) -> &'static str {
        match self {
            JobState::Pending => "PENDING",
            JobState::Running => "RUNNING",
            JobState::Succeeded => "SUCCEEDED",
            JobState::Failed => "FAILED",
            JobState::Cancelled => "CANCELLED",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled
        )
    }

    pub fn transition(self, to: JobState) -> Result<JobState, TransitionError> {
        match (self, to) {
            (JobState::Pending, JobState::Running)
            | (JobState::Pending, JobState::Cancelled)
            | (JobState::Running, JobState::Succeeded)
            | (JobState::Running, JobState::Failed)
            | (JobState::Running, JobState::Cancelled) => Ok(to),
            _ => Err(TransitionError { from: self, to }),
        }
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StateParseError {
    #[error("unknown job state {0:?}")]
    Unknown(String),
}

impl FromStr for JobState {
    type Err = StateParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "PENDING" => Ok(JobState::Pending),
            "RUNNING" => Ok(JobState::Running),
            "SUCCEEDED" => Ok(JobState::Succeeded),
            "FAILED" => Ok(JobState::Failed),
            "CANCELLED" => Ok(JobState::Cancelled),
            other => Err(StateParseError::Unknown(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("transition from {from} to {to} is not allowed")]
pub struct TransitionError {
    pub from: JobState,
    pub to: JobState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Running;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Succeeded;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

pub trait Stage: private::Sealed {
    const STATE: JobState;
}

mod private {
    pub trait Sealed {}

    impl Sealed for super::Pending {}
    impl Sealed for super::Running {}
    impl Sealed for super::Succeeded {}
    impl Sealed for super::Failed {}
    impl Sealed for super::Cancelled {}
}

impl Stage for Pending {
    const STATE: JobState = JobState::Pending;
}

impl Stage for Running {
    const STATE: JobState = JobState::Running;
}

impl Stage for Succeeded {
    const STATE: JobState = JobState::Succeeded;
}

impl Stage for Failed {
    const STATE: JobState = JobState::Failed;
}

impl Stage for Cancelled {
    const STATE: JobState = JobState::Cancelled;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job<S: Stage> {
    id: JobId,
    owner: UserId,
    request: RequestId,
    stage: S,
}

impl<S: Stage> Job<S> {
    pub fn id(&self) -> JobId {
        self.id
    }

    pub fn owner(&self) -> &UserId {
        &self.owner
    }

    pub fn request(&self) -> &RequestId {
        &self.request
    }

    pub fn state(&self) -> JobState {
        S::STATE
    }

    fn into_stage<T: Stage>(self, stage: T) -> Job<T> {
        Job {
            id: self.id,
            owner: self.owner,
            request: self.request,
            stage,
        }
    }
}

impl Job<Pending> {
    pub fn create(id: JobId, owner: UserId, request: RequestId) -> Self {
        Job {
            id,
            owner,
            request,
            stage: Pending,
        }
    }

    pub fn start(self) -> Job<Running> {
        self.into_stage(Running)
    }

    pub fn cancel(self) -> Job<Cancelled> {
        self.into_stage(Cancelled)
    }
}

impl Job<Running> {
    pub fn succeed(self) -> Job<Succeeded> {
        self.into_stage(Succeeded)
    }

    pub fn fail(self) -> Job<Failed> {
        self.into_stage(Failed)
    }

    pub fn cancel(self) -> Job<Cancelled> {
        self.into_stage(Cancelled)
    }
}
