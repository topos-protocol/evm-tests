//! Handles feeding the parsed tests into `plonky2` and determining the result.
//! Essentially converts parsed test into test results.

use std::panic;

use common::types::ParsedTest;
use ethereum_types::{BigEndianHash, H256};
use plonky2::{
    field::goldilocks_field::GoldilocksField, plonk::config::KeccakGoldilocksConfig,
    util::timing::TimingTree,
};
use plonky2_evm::{all_stark::AllStark, config::StarkConfig, prover::prove};

use crate::test_dir_reading::{ParsedTestGroup, ParsedTestSubGroup, Test};

#[derive(Debug)]
pub(crate) enum TestStatus {
    Passed,
    EvmErr(String),
    EvmPanic(String),
    IncorrectAccountFinalState(H256, H256),
}

#[derive(Debug)]
pub(crate) struct TestGroupRunResults {
    pub(crate) name: String,
    pub(crate) sub_group_res: Vec<TestSubGroupRunResults>,
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

pub(crate) fn run_plonky2_tests(parsed_tests: Vec<ParsedTestGroup>) -> Vec<TestGroupRunResults> {
    parsed_tests.into_iter().map(run_test_group).collect()
}

fn run_test_group(group: ParsedTestGroup) -> TestGroupRunResults {
    TestGroupRunResults {
        name: group.name,
        sub_group_res: group
            .sub_groups
            .into_iter()
            .map(run_test_sub_group)
            .collect(),
    }
}

fn run_test_sub_group(sub_group: ParsedTestSubGroup) -> TestSubGroupRunResults {
    TestSubGroupRunResults {
        name: sub_group.name,
        test_res: sub_group.tests.into_iter().map(run_test).collect(),
    }
}

fn run_test(test: Test) -> TestRunResult {
    let res = run_test_and_get_test_result(test.info);
    TestRunResult {
        name: test.name,
        status: res,
    }
}

/// Run a test against `plonky2` and output a result based on what happens.
fn run_test_and_get_test_result(test: ParsedTest) -> TestStatus {
    let proof_run_res = panic::catch_unwind(|| {
        prove::<GoldilocksField, KeccakGoldilocksConfig, 2>(
            &AllStark::default(),
            &StarkConfig::standard_fast_config(),
            test.plonky2_inputs,
            &mut TimingTree::default(),
        )
    });

    let proof_run_output = match proof_run_res {
        Ok(Ok(res)) => res,
        Ok(Err(err)) => return TestStatus::EvmErr(err.to_string()),
        Err(err) => {
            let panic_str = match err.downcast::<String>() {
                Ok(panic_str) => *panic_str,
                Err(_) => "Unknown panic reason.".to_string(),
            };

            return TestStatus::EvmPanic(panic_str);
        }
    };

    // TODO: Remove `U256` --> `H256` conversion once `plonky2` switches over to
    // `H256`...
    let final_state_trie_hash = H256::from_uint(&ethereum_types::U256(
        proof_run_output.public_values.trie_roots_after.state_root.0,
    ));
    if let Some(expected_state_trie_hash) = test.expected_final_account_states && final_state_trie_hash != expected_state_trie_hash {
        return TestStatus::IncorrectAccountFinalState(final_state_trie_hash, expected_state_trie_hash)
    }

    TestStatus::Passed
}
