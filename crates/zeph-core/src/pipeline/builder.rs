use super::PipelineError;
use super::step::Step;

pub trait Runnable: Send + Sync {
    type Input: Send;
    type Output: Send;

    fn run(
        &self,
        input: Self::Input,
    ) -> impl std::future::Future<Output = Result<Self::Output, PipelineError>> + Send;
}

pub struct Start<S>(S);

impl<S: Step> Runnable for Start<S> {
    type Input = S::Input;
    type Output = S::Output;

    async fn run(&self, input: Self::Input) -> Result<Self::Output, PipelineError> {
        self.0.run(input).await
    }
}

pub struct Chain<Prev, Current> {
    prev: Prev,
    current: Current,
}

impl<Prev, Current> Runnable for Chain<Prev, Current>
where
    Prev: Runnable,
    Current: Step<Input = Prev::Output>,
{
    type Input = Prev::Input;
    type Output = Current::Output;

    async fn run(&self, input: Self::Input) -> Result<Self::Output, PipelineError> {
        let intermediate = self.prev.run(input).await?;
        self.current.run(intermediate).await
    }
}

pub struct Pipeline<S> {
    steps: S,
}

impl Pipeline<()> {
    #[must_use]
    pub fn start<S: Step>(step: S) -> Pipeline<Start<S>> {
        Pipeline { steps: Start(step) }
    }
}

impl<S> Pipeline<S> {
    #[must_use]
    pub fn step<T: Step>(self, step: T) -> Pipeline<Chain<S, T>> {
        Pipeline {
            steps: Chain {
                prev: self.steps,
                current: step,
            },
        }
    }
}

impl<S: Runnable> Pipeline<S> {
    /// # Errors
    ///
    /// Returns `PipelineError` if any step in the pipeline fails.
    pub async fn run(&self, input: S::Input) -> Result<S::Output, PipelineError> {
        self.steps.run(input).await
    }
}
