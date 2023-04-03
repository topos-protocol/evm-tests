//! Handles feeding the parsed tests into `plonky2` and determining the result.
//! Essentially converts parsed tests into test results.

use std::{fmt::Display, sync::atomic::Ordering, time::Duration};

use common::types::TestVariantRunInfo;
use ethereum_types::H256;
use indicatif::{ProgressBar, ProgressStyle};
use log::trace;
use plonky2::{
    field::goldilocks_field::GoldilocksField, plonk::config::KeccakGoldilocksConfig,
    util::timing::TimingTree,
};
use plonky2_evm::{all_stark::AllStark, config::StarkConfig, prover::prove_with_outputs};

use crate::{
    persistent_run_state::TestRunEntries,
    state_diff::StateDiff,
    test_dir_reading::{ParsedTestGroup, ParsedTestSubGroup, Test},
    ProcessAbortedRecv,
};

pub(crate) type RunnerResult<T> = Result<T, ()>;

trait TestProgressIndicator {
    fn set_current_test_name(&self, t_name: String);
    fn notify_test_completed(&mut self);
}

/// Simple test progress indicator that uses `println!`s.
struct SimpleProgressIndicator {
    num_tests: u64,
    curr_test: usize,
}

impl TestProgressIndicator for SimpleProgressIndicator {
    fn set_current_test_name(&self, t_name: String) {
        println!(
            "({}/{}) Running {}...",
            self.curr_test, self.num_tests, t_name
        );
    }

    // Kinda gross...
    fn notify_test_completed(&mut self) {
        self.curr_test += 1;
    }
}

/// More elegant test progress indicator that uses a progress bar library.
struct FancyProgressIndicator {
    prog_bar: ProgressBar,
}

impl TestProgressIndicator for FancyProgressIndicator {
    fn set_current_test_name(&self, t_name: String) {
        self.prog_bar.set_message(t_name);
    }

    fn notify_test_completed(&mut self) {
        self.prog_bar.inc(1);
    }
}

#[derive(Clone, Debug)]
pub(crate) enum TestStatus {
    Passed,
    EvmErr(String),
    IncorrectAccountFinalState(TrieFinalStateDiff),
}

impl Display for TestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestStatus::Passed => write!(f, "Passed"),
            TestStatus::EvmErr(err) => write!(f, "Evm error: {}", err),
            TestStatus::IncorrectAccountFinalState(diff) => {
                write!(f, "Expected trie hash mismatch: {}", diff)
            }
        }
    }
}

/// If one or more trie hashes are different from the expected, then we return a
/// diff showing which tries where different.
#[derive(Clone, Debug)]
pub(crate) struct TrieFinalStateDiff {
    state: TrieComparisonResult,
    receipt: TrieComparisonResult,
    transaction: TrieComparisonResult,
}

/// A result of comparing the actual outputted `plonky2` trie to the one
/// expected by the test.
#[derive(Clone, Debug)]
enum TrieComparisonResult {
    Correct,
    Difference(H256, H256),
}

impl Display for TrieComparisonResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Correct => write!(f, "Correct"),
            Self::Difference(actual, expected) => {
                write!(f, "Difference (Actual: {}, Expected: {})", actual, expected)
            }
        }
    }
}

impl Display for TrieFinalStateDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "State: {}, Receipt: {}, Transaction: {}",
            self.state, self.receipt, self.transaction
        )
    }
}

impl TestStatus {
    pub(crate) fn passed(&self) -> bool {
        matches!(self, TestStatus::Passed)
    }
}

#[derive(Debug)]
pub(crate) struct TestGroupRunResults {
    pub(crate) name: String,
    pub(crate) sub_group_res: Vec<TestSubGroupRunResults>,
}

fn num_tests_in_groups<'a>(groups: impl Iterator<Item = &'a ParsedTestGroup> + 'a) -> u64 {
    groups
        .map(|g| {
            g.sub_groups
                .iter()
                .flat_map(|sub_g| sub_g.tests.iter())
                .count() as u64
        })
        .sum()
}

#[derive(Debug)]
pub(crate) struct TestSubGroupRunResults {
    pub(crate) name: String,
    pub(crate) test_res: Vec<TestRunResult>,
}

#[derive(Debug)]
pub(crate) struct TestRunResult {
    pub(crate) name: String,
    pub(crate) status: TestStatus,
}

pub(crate) fn run_plonky2_tests(
    parsed_tests: Vec<ParsedTestGroup>,
    simple_progress_indicator: bool,
    persistent_test_state: &mut TestRunEntries,
    mut process_aborted: ProcessAbortedRecv,
) -> RunnerResult<Vec<TestGroupRunResults>> {
    let num_tests = num_tests_in_groups(parsed_tests.iter());
    let mut p_indicator = create_progress_indicator(num_tests, simple_progress_indicator);

    parsed_tests
        .into_iter()
        .map(|g| {
            run_test_group(
                g,
                &mut p_indicator,
                persistent_test_state,
                &mut process_aborted,
            )
        })
        .collect::<RunnerResult<_>>()
}

fn create_progress_indicator(
    num_tests: u64,
    simple_progress_indicator: bool,
) -> Box<dyn TestProgressIndicator> {
    match simple_progress_indicator {
        false => Box::new({
            FancyProgressIndicator {
                prog_bar: ProgressBar::new(num_tests).with_style(
                    ProgressStyle::with_template(
                        "{bar:60.magenta} {pos}/{len} ETA: [{eta_precise}] | Test: {msg}",
                    )
                    .unwrap(),
                ),
            }
        }),
        true => Box::new(SimpleProgressIndicator {
            curr_test: 0,
            num_tests,
        }),
    }
}

fn run_test_group(
    group: ParsedTestGroup,
    p_indicator: &mut Box<dyn TestProgressIndicator>,
    persistent_test_state: &mut TestRunEntries,
    process_aborted: &mut ProcessAbortedRecv,
) -> RunnerResult<TestGroupRunResults> {
    Ok(TestGroupRunResults {
        name: group.name,
        sub_group_res: group
            .sub_groups
            .into_iter()
            .map(|sub_g| {
                run_test_sub_group(sub_g, p_indicator, persistent_test_state, process_aborted)
            })
            .collect::<RunnerResult<_>>()?,
    })
}

fn run_test_sub_group(
    sub_group: ParsedTestSubGroup,
    p_indicator: &mut Box<dyn TestProgressIndicator>,
    persistent_test_state: &mut TestRunEntries,
    process_aborted: &mut ProcessAbortedRecv,
) -> RunnerResult<TestSubGroupRunResults> {
    Ok(TestSubGroupRunResults {
        name: sub_group.name,
        test_res: sub_group
            .tests
            .into_iter()
            .map(|sub_g| run_test(sub_g, p_indicator, persistent_test_state, process_aborted))
            .collect::<RunnerResult<_>>()?,
    })
}

fn run_test(
    test: Test,
    p_indicator: &mut Box<dyn TestProgressIndicator>,
    persistent_test_state: &mut TestRunEntries,
    process_aborted: &ProcessAbortedRecv,
) -> RunnerResult<TestRunResult> {
    trace!("Running test {}...", test.name);

    p_indicator.set_current_test_name(test.name.to_string());
    let res = run_test_and_get_test_result(test.info);

    if process_aborted.load(Ordering::Relaxed) {
        // Stop running more tests.
        return Err(());
    }

    persistent_test_state.update_test_state(&test.name, res.clone().into());
    p_indicator.notify_test_completed();

    Ok(TestRunResult {
        name: test.name,
        status: res,
    })
}

/// Run a test against `plonky2` and output a result based on what happens.
fn run_test_and_get_test_result(test: TestVariantRunInfo) -> TestStatus {
    let timing = TimingTree::new("prove", log::Level::Debug);

    let proof_run_res = prove_with_outputs::<GoldilocksField, KeccakGoldilocksConfig, 2>(
        &AllStark::default(),
        &StarkConfig::standard_fast_config(),
        test.gen_inputs,
        &mut TimingTree::default(),
    );

    timing.filter(Duration::from_millis(100)).print();

    let (proof_run_output, generation_outputs) = match proof_run_res {
        Ok(v) => v,
        Err(evm_err) => return TestStatus::EvmErr(evm_err.to_string()),
    };

    let actual_state_trie_hash = proof_run_output.public_values.trie_roots_after.state_root;
    if actual_state_trie_hash != test.common.expected_final_account_state_root_hash {
        if let Some(serialized_revm_variant) = test.revm_variant {
            let instance = serialized_revm_variant.into_hydrated();
            let expected_state = instance.transact_ref().map(|result| result.state);
            if let Ok(state) = expected_state {
                let state_diff = StateDiff::new(state, generation_outputs.accounts);
                // TODO: Make this optional / configurable
                println!("{}", state_diff);
            }
        }

        let trie_diff = TrieFinalStateDiff {
            state: TrieComparisonResult::Difference(
                actual_state_trie_hash,
                test.common.expected_final_account_state_root_hash,
            ),
            receipt: TrieComparisonResult::Correct, // TODO...
            transaction: TrieComparisonResult::Correct, // TODO...
        };

        return TestStatus::IncorrectAccountFinalState(trie_diff);
    }

    // TODO: Also check receipt and txn hashes once these are provided by the
    // parser...

    TestStatus::Passed
}
