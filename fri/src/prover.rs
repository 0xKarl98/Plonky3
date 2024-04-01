use alloc::vec;
use alloc::vec::Vec;

use itertools::Itertools;
use p3_challenger::{CanObserve, CanSample, GrindingChallenger};
use p3_commit::Mmcs;
use p3_field::Field;
use p3_matrix::dense::RowMajorMatrix;
use tracing::{info_span, instrument};

use crate::{CommitPhaseProofStep, FriConfig, FriFolder, FriProof, QueryProof};

#[instrument(name = "FRI prover", skip_all)]
pub fn prove<Folder, F, M, Challenger>(
    config: &FriConfig<M>,
    folder: &Folder,
    input: &[Option<Vec<F>>; 32],
    challenger: &mut Challenger,
) -> (FriProof<F, M, Challenger::Witness>, Vec<usize>)
where
    F: Field,
    M: Mmcs<F>,
    Challenger: GrindingChallenger + CanObserve<M::Commitment> + CanSample<F>,
    Folder: FriFolder<F>,
{
    let log_max_height = input.iter().rposition(Option::is_some).unwrap();

    let commit_phase_result = commit_phase(config, folder, input, log_max_height, challenger);

    let pow_witness = challenger.grind(config.proof_of_work_bits);

    let query_indices: Vec<usize> = (0..config.num_queries)
        .map(|_| challenger.sample_bits(log_max_height))
        .collect();

    let query_proofs = info_span!("query phase").in_scope(|| {
        query_indices
            .iter()
            .map(|&index| answer_query(config, &commit_phase_result.data, index))
            .collect()
    });

    (
        FriProof {
            commit_phase_commits: commit_phase_result.commits,
            query_proofs,
            final_poly: commit_phase_result.final_poly,
            pow_witness,
        },
        query_indices,
    )
}

fn answer_query<F, M>(
    config: &FriConfig<M>,
    commit_phase_commits: &[M::ProverData<RowMajorMatrix<F>>],
    index: usize,
) -> QueryProof<F, M>
where
    F: Field,
    M: Mmcs<F>,
{
    let commit_phase_openings = commit_phase_commits
        .iter()
        .enumerate()
        .map(|(i, prover_data)| {
            let index_i = index >> i;
            let index_i_sibling = index_i ^ 1;
            let index_pair = index_i >> 1;

            let (mut opened_rows, opening_proof) = config.mmcs.open_batch(index_pair, prover_data);
            assert_eq!(opened_rows.len(), 1);
            let opened_row = opened_rows.pop().unwrap();
            assert_eq!(opened_row.len(), 2, "Committed data should be in pairs");
            let sibling_value = opened_row[index_i_sibling % 2];

            CommitPhaseProofStep {
                sibling_value,
                opening_proof,
            }
        })
        .collect();

    QueryProof {
        commit_phase_openings,
    }
}

#[instrument(name = "commit phase", skip_all)]
fn commit_phase<Folder, F, M, Challenger>(
    config: &FriConfig<M>,
    folder: &Folder,
    input: &[Option<Vec<F>>; 32],
    log_max_height: usize,
    challenger: &mut Challenger,
) -> CommitPhaseResult<F, M, RowMajorMatrix<F>>
where
    F: Field,
    M: Mmcs<F>,
    Challenger: CanObserve<M::Commitment> + CanSample<F>,
    Folder: FriFolder<F>,
{
    let mut current = input[log_max_height].as_ref().unwrap().clone();

    let mut commits = vec![];
    let mut data = vec![];

    for log_folded_height in (config.log_blowup..log_max_height).rev() {
        let leaves = RowMajorMatrix::new(current, 2);
        let (commit, prover_data) = config.mmcs.commit_matrix(leaves);
        challenger.observe(commit.clone());

        let beta: F = challenger.sample();
        // we passed ownership of `current` to the MMCS, so get a reference to it
        let leaves = config.mmcs.get_matrices(&prover_data).pop().unwrap();
        current = Folder::fold_matrix(leaves.as_view(), beta);

        commits.push(commit);
        data.push(prover_data);

        if let Some(v) = &input[log_folded_height] {
            // current.iter_mut().zip_eq(v).for_each(|(c, v)| *c += *v);
            folder.combine_vec(&mut current, v);
        }
    }

    // We should be left with `blowup` evaluations of a constant polynomial.
    assert_eq!(current.len(), config.blowup());
    let final_poly = current[0];
    for x in current {
        assert_eq!(x, final_poly);
    }

    CommitPhaseResult {
        commits,
        data,
        final_poly,
    }
}

struct CommitPhaseResult<F, M: Mmcs<F>, Mat> {
    commits: Vec<M::Commitment>,
    data: Vec<M::ProverData<Mat>>,
    final_poly: F,
}
