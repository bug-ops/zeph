use std::future::Future;

use super::PipelineError;

pub trait Step: Send + Sync {
    type Input: Send;
    type Output: Send;

    fn run(
        &self,
        input: Self::Input,
    ) -> impl Future<Output = Result<Self::Output, PipelineError>> + Send;
}
