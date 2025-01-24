use alloc::vec;
use alloc::vec::Vec;

use itertools::{izip, Itertools};
use p3_air::Air;
use p3_challenger::{CanObserve, CanSample, FieldChallenger};
use p3_commit::{Pcs, PolynomialSpace};
use p3_field::Field;
use p3_field::{FieldAlgebra, FieldExtensionAlgebra, PackedValue};
use p3_matrix::dense::RowMajorMatrix;
use p3_matrix::Matrix;
use p3_maybe_rayon::prelude::*;
use p3_util::{log2_ceil_usize, log2_strict_usize};
use tracing::{info_span, instrument};

use crate::{
    get_symbolic_constraints, Commitments, Domain, OpenedValues, PackedChallenge, PackedVal, Proof,
    ProverConstraintFolder, StarkGenericConfig, SymbolicAirBuilder, SymbolicExpression, Val,
};

#[instrument(skip_all)]
#[allow(clippy::multiple_bound_locations)] // cfg not supported in where clauses?
pub fn prove<
    SC,
    #[cfg(debug_assertions)] A: for<'a> Air<crate::check_constraints::DebugConstraintBuilder<'a, Val<SC>>>,
    #[cfg(not(debug_assertions))] A,
>(
    config: &SC,
    air: &A,
    challenger: &mut SC::Challenger,
    trace: RowMajorMatrix<Val<SC>>,
    public_values: &Vec<Val<SC>>,
) -> Proof<SC>
where
    SC: StarkGenericConfig,
    A: Air<SymbolicAirBuilder<Val<SC>>> + for<'a> Air<ProverConstraintFolder<'a, SC>>,
{
    #[cfg(debug_assertions)]
    crate::check_constraints::check_constraints(air, &trace, public_values);

    let degree = trace.height();
    let log_degree = log2_strict_usize(degree);

    let symbolic_constraints = get_symbolic_constraints::<Val<SC>, A>(air, 0, public_values.len());
    let constraint_count = symbolic_constraints.len();
    let constraint_degree = symbolic_constraints
        .iter()
        .map(SymbolicExpression::degree_multiple)
        .max()
        .unwrap_or(0);
    let log_quotient_degree = log2_ceil_usize(constraint_degree - 1);
    let quotient_degree = 1 << log_quotient_degree;

    let pcs = config.pcs();
    let trace_domain = pcs.natural_domain_for_degree(degree);

    let (trace_commit, trace_data) =
        info_span!("commit to trace data").in_scope(|| pcs.commit(vec![(trace_domain, trace)]));

    // Observe the instance.
    challenger.observe(Val::<SC>::from_canonical_usize(log_degree));
    // TODO: Might be best practice to include other instance data here; see verifier comment.

    challenger.observe(trace_commit.clone());
    challenger.observe_slice(public_values);
    let alpha: SC::Challenge = challenger.sample_ext_element();

    let quotient_domain =
        trace_domain.create_disjoint_domain(1 << (log_degree + log_quotient_degree));

    let trace_on_quotient_domain = pcs.get_evaluations_on_domain(&trace_data, 0, quotient_domain);

    let quotient_values = quotient_values(
        air,
        public_values,
        trace_domain,
        quotient_domain,
        trace_on_quotient_domain,
        alpha,
        constraint_count,
    );
    let quotient_flat = RowMajorMatrix::new_col(quotient_values.clone()).flatten_to_base();
    let quotient_chunks = quotient_domain.split_evals(quotient_degree, quotient_flat);
    let qc_domains = quotient_domain.split_domains(quotient_degree);

    // Compute the vanishing polynomial normalizing constants, based on the verifier's check.
    let zp_cis = qc_domains
        .iter()
        .enumerate()
        .map(|(i, domain)| {
            qc_domains
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(j, other_domain)| other_domain.zp_at_point(domain.first_point()).inverse())
                .product()
        })
        .collect_vec();
    let (quotient_commit, quotient_data) =
        info_span!("commit to quotient poly chunks").in_scope(|| {
            pcs.commit_quotient(
                izip!(qc_domains.clone(), quotient_chunks.clone()).collect_vec(),
                zp_cis.clone(),
            )
        });
    challenger.observe(quotient_commit.clone());

    let mut random_vals = if pcs.is_zk() {
        Some(pcs.generate_random_vals(trace_domain.size()))
        // None
    } else {
        None
    };
    random_vals = None;

    let opt_random_commit = if let Some(r_vs) = random_vals {
        let extended_domain = pcs.natural_domain_for_degree(trace_domain.size() * 2);
        Some(pcs.commit(vec![(extended_domain, r_vs)]))
        // None
    } else {
        None
    };
    /////////////////////////// Linda debug
    // Check whether the final verification check would pass: Checking that
    // the sum of quotients * vanishing polynomial would give the same result for the original and current values.
    // Get opened values for original and randomized quotient chunks.
    let pt: SC::Challenge = challenger.sample();
    let (_, orig_quotient_data) =
        pcs.commit(izip!(qc_domains.clone(), quotient_chunks).collect_vec());
    let (opened_vals_quo, _) = pcs.open(
        vec![
            (
                &orig_quotient_data,
                (0..quotient_degree).map(|_| vec![pt]).collect_vec(),
            ),
            (
                &quotient_data,
                (0..quotient_degree).map(|_| vec![pt]).collect_vec(),
            ),
        ],
        challenger,
    );

    let zps = qc_domains
        .iter()
        .enumerate()
        .map(|(i, domain)| {
            qc_domains
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, other_domain)| {
                    // ((generator * other_domain.first_point().inverse()).exp_u64(other_domain.size() as u64) - SC::Challenge::ONE)
                    other_domain.zp_at_point(pt)
                        * other_domain.zp_at_point(domain.first_point()).inverse()
                })
                .product::<SC::Challenge>()
        })
        .collect_vec();
    let opened_1 = opened_vals_quo[0]
        .iter()
        .map(|v| v[0].clone())
        .collect_vec();
    let opened_2 = opened_vals_quo[1]
        .iter()
        .map(|v| v[0].clone())
        .collect_vec();

    let quotient_orig = opened_1
        .iter()
        .enumerate()
        .map(|(ch_i, ch)| {
            ch.iter()
                .enumerate()
                .map(|(e_i, &c)| zps[ch_i] * SC::Challenge::monomial(e_i) * c)
                .sum::<SC::Challenge>()
        })
        .sum::<SC::Challenge>();

    let quotient_rand = opened_2
        .iter()
        .enumerate()
        .map(|(ch_i, ch)| {
            ch.iter()
                .enumerate()
                .map(|(e_i, &c)| zps[ch_i] * SC::Challenge::monomial(e_i) * c)
                .sum::<SC::Challenge>()
        })
        .sum::<SC::Challenge>();

    let diff = quotient_rand - quotient_orig;

    assert_eq!(
        quotient_orig, quotient_rand,
        "orig val {:?}, rand val {:?}, diff {:?}",
        quotient_orig, quotient_rand, diff
    );
    ////////////////////////////////////

    let commitments = Commitments {
        trace: trace_commit,
        quotient_chunks: quotient_commit,
    };

    let zeta: SC::Challenge = challenger.sample();
    let zeta_next = trace_domain.next_point(zeta).unwrap();

    let (opened_values, opening_proof) = info_span!("open").in_scope(|| {
        if let Some((r_commit, r_data)) = opt_random_commit {
            pcs.open(
                vec![
                    (&trace_data, vec![vec![zeta, zeta_next]]),
                    (
                        &quotient_data,
                        // open every chunk at zeta
                        (0..quotient_degree).map(|_| vec![zeta]).collect_vec(),
                    ),
                    (&r_data, vec![vec![zeta]]),
                ],
                challenger,
            )
        } else {
            pcs.open(
                vec![
                    (&trace_data, vec![vec![zeta, zeta_next]]),
                    (
                        &quotient_data,
                        // open every chunk at zeta
                        (0..quotient_degree).map(|_| vec![zeta]).collect_vec(),
                    ),
                ],
                challenger,
            )
        }
    });
    let trace_local = opened_values[0][0][0].clone();
    let trace_next = opened_values[0][0][1].clone();
    let quotient_chunks = opened_values[1].iter().map(|v| v[0].clone()).collect_vec();
    let opened_values = OpenedValues {
        trace_local,
        trace_next,
        quotient_chunks,
    };
    Proof {
        commitments,
        opened_values,
        opening_proof,
        degree_bits: log_degree,
    }
}

#[instrument(name = "compute quotient polynomial", skip_all)]
fn quotient_values<SC, A, Mat>(
    air: &A,
    public_values: &Vec<Val<SC>>,
    trace_domain: Domain<SC>,
    quotient_domain: Domain<SC>,
    trace_on_quotient_domain: Mat,
    alpha: SC::Challenge,
    constraint_count: usize,
) -> Vec<SC::Challenge>
where
    SC: StarkGenericConfig,
    A: for<'a> Air<ProverConstraintFolder<'a, SC>>,
    Mat: Matrix<Val<SC>> + Sync,
{
    let quotient_size = quotient_domain.size();
    let width = trace_on_quotient_domain.width();
    let mut sels = trace_domain.selectors_on_coset(quotient_domain);

    let qdb = log2_strict_usize(quotient_domain.size()) - log2_strict_usize(trace_domain.size());
    let next_step = 1 << qdb;

    // We take PackedVal::<SC>::WIDTH worth of values at a time from a quotient_size slice, so we need to
    // pad with default values in the case where quotient_size is smaller than PackedVal::<SC>::WIDTH.
    for _ in quotient_size..PackedVal::<SC>::WIDTH {
        sels.is_first_row.push(Val::<SC>::default());
        sels.is_last_row.push(Val::<SC>::default());
        sels.is_transition.push(Val::<SC>::default());
        sels.inv_zeroifier.push(Val::<SC>::default());
    }

    let mut alpha_powers = alpha.powers().take(constraint_count).collect_vec();
    alpha_powers.reverse();

    (0..quotient_size)
        .into_par_iter()
        .step_by(PackedVal::<SC>::WIDTH)
        .flat_map_iter(|i_start| {
            let i_range = i_start..i_start + PackedVal::<SC>::WIDTH;

            let is_first_row = *PackedVal::<SC>::from_slice(&sels.is_first_row[i_range.clone()]);
            let is_last_row = *PackedVal::<SC>::from_slice(&sels.is_last_row[i_range.clone()]);
            let is_transition = *PackedVal::<SC>::from_slice(&sels.is_transition[i_range.clone()]);
            let inv_zeroifier = *PackedVal::<SC>::from_slice(&sels.inv_zeroifier[i_range.clone()]);

            let main = RowMajorMatrix::new(
                trace_on_quotient_domain.vertically_packed_row_pair(i_start, next_step),
                width,
            );

            let accumulator = PackedChallenge::<SC>::ZERO;
            let mut folder = ProverConstraintFolder {
                main: main.as_view(),
                public_values,
                is_first_row,
                is_last_row,
                is_transition,
                alpha_powers: &alpha_powers,
                accumulator,
                constraint_index: 0,
            };
            air.eval(&mut folder);

            // quotient(x) = constraints(x) / Z_H(x)
            let quotient = folder.accumulator * inv_zeroifier;

            // "Transpose" D packed base coefficients into WIDTH scalar extension coefficients.
            (0..core::cmp::min(quotient_size, PackedVal::<SC>::WIDTH)).map(move |idx_in_packing| {
                SC::Challenge::from_base_fn(|coeff_idx| {
                    quotient.as_base_slice()[coeff_idx].as_slice()[idx_in_packing]
                })
            })
        })
        .collect()
}
