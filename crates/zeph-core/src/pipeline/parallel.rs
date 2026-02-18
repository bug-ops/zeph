use super::PipelineError;
use super::step::Step;

pub struct ParallelStep<A, B> {
    a: A,
    b: B,
}

#[must_use]
pub fn parallel<A, B>(a: A, b: B) -> ParallelStep<A, B> {
    ParallelStep { a, b }
}

impl<A, B> Step for ParallelStep<A, B>
where
    A: Step,
    B: Step<Input = A::Input>,
    A::Input: Clone,
{
    type Input = A::Input;
    type Output = (A::Output, B::Output);

    async fn run(&self, input: Self::Input) -> Result<Self::Output, PipelineError> {
        let input_b = input.clone();
        let (ra, rb) = tokio::join!(self.a.run(input), self.b.run(input_b));
        Ok((ra?, rb?))
    }
}
