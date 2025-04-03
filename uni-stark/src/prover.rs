//! Produce a proof that the given trace satisfies the given air.
//!
//! While this implementation, is designed to work with different proof schemes (Both the regular stark and the circle stark)
//! for simplicities sake, we focus on the regular stark proof scheme here. Information about the circle stark proof scheme
//! can be found in the paper https://eprint.iacr.org/2024/278.pdf.
//!
//! TODO: Add a similar overview of the circle stark proof scheme.
//!
//! Standard STARK:
//!
//! Definitions and Setup:
//! - Fix a field `F` with cryptographically large extension field `G`.
//! - Let `T` denote the trace of the computation. It is a matrix of height `N = 2^n` and width `l`.
//! - Let `H = <h>` denote a multiplicative subgroup of `F` of size `2^n` with generator `h`.
//! - Given the `i`th trace column `Ti`, we let `Ti(x)` denote the unique polynomial of degree `N`
//!     such that `Ti(h^j) = Ti[j]` for `j` in `0..N`.
//!     In other words, `Ti(x)` is the evaluation vector of `Ti(x)` over `H`.
//! - Let `C(X1, ..., Xl, Y1, ..., Yl, Z1, ..., Zj)` denote the constraint polynomial coming from the AIR.
//!     It depends on both the current row and the next row and a collection of selector polynomials.
//! - Given a polynomial `f` and a set `D`, let `[[f, D]]` denote a merkle commitment to
//!     the evaluation vector of `f` over `D`. Similarly, `[[{f0, ..., fk}, D]]` denotes a combined merkle
//!     commitment to the evaluation vectors of the polynomials `f0, ..., fk` over `D`.
//!
//! The goal of the prover is to produce a proof that it knows a trace `T` such that:
//! `C(T1(x), ..., Tl(x), T1(gx), ..., Tl(gx), selectors) = 0` for all `x` in `H`.
//!
//! Proof Overview:
//!
//! We start by fixing a pair of elements `g0, g1` in `F\H` such that the cosets `g0 H` and `g1 H` are distinct.
//! Let `D` denote the union of those two cosets. Then, for every column `i`, the prover computes the evaluation vectors
//! of `Ti(x)` over `D`. The prover makes a combined merkle commitment `[[{T1, ..., Tl}, D]]`
//! to these vectors and sends it to `V`.
//!
//! The prover is telling the truth if and only if there exists a polynomial `Q` of degree `< deg(C) * (N - 1) - N`
//! such that `Q(x) = C(T1(x), ..., Tl(x), T1(gx), ..., Tl(gx))/ZH(x)` where `ZH(x) = x^N - 1` is a vanishing polynomial
//! of the subgroup `H`.
//!
//! The prover uses `C` and `Ti` to compute the evaluations of the quotient polynomial `Q` over `D`.
//!

use alloc::vec;
use alloc::vec::Vec;

use itertools::{Itertools, izip};
use p3_air::Air;
use p3_challenger::{CanObserve, CanSample, FieldChallenger};
use p3_commit::{Pcs, PolynomialSpace};
use p3_field::{BasedVectorSpace, PackedValue, PrimeCharacteristicRing};
use p3_matrix::Matrix;
use p3_matrix::dense::RowMajorMatrix;
use p3_maybe_rayon::prelude::*;
use p3_util::{log2_ceil_usize, log2_strict_usize};
use tracing::{debug_span, info_span, instrument};

use crate::{
    Commitments, Domain, OpenedValues, PackedChallenge, PackedVal, Proof, ProverConstraintFolder,
    StarkGenericConfig, SymbolicAirBuilder, SymbolicExpression, Val, get_symbolic_constraints,
};

/// Produce a proof that the given trace satisfies the given air.
///
/// Arguments:
/// Config: A collection of public data about the shape of the proof. It includes:
///     - A Polynomial Commitment Scheme.
///     - An Extension field from which random challenges are drawn.
///     - A Random Challenger used for the Fiat-Shamir implementation.
///     - TODO: Should this contain parts of the fri config? E.g. log_blowup?
///
/// air: TODO
/// trace: The execution trace to be proven:
///     - A matrix of height `N = 2^n` and width `l`.
///     - Each column `Ti` is interpreted as an evaluation vector of a polynomial `Ti(x)` over the initial domain `H`.         
/// public_values: A list of public values related to the proof.
///     - TODO: Should this be absorbed into SC?
#[instrument(skip_all)]
#[allow(clippy::multiple_bound_locations)] // cfg not supported in where clauses?
pub fn prove<
    SC,
    #[cfg(debug_assertions)] A: for<'a> Air<crate::check_constraints::DebugConstraintBuilder<'a, Val<SC>>>,
    #[cfg(not(debug_assertions))] A,
>(
    config: &SC,
    air: &A,
    trace: RowMajorMatrix<Val<SC>>,
    public_values: &Vec<Val<SC>>,
) -> Proof<SC>
where
    SC: StarkGenericConfig,
    A: Air<SymbolicAirBuilder<Val<SC>>> + for<'a> Air<ProverConstraintFolder<'a, SC>>,
{
    // In debug mode, check that every row of the trace satisfies the constraint polynomial.
    #[cfg(debug_assertions)]
    crate::check_constraints::check_constraints(air, &trace, public_values);

    // Compute the height `N = 2^n` and `log_2(height)`, `n`, of the trace.
    let degree = trace.height();
    let log_degree = log2_strict_usize(degree);

    // Find deg(C), the degree of the constraint polynomial.
    // For now let us assume that `deg(C) = 3`. TODO: Generalize this assumption.
    let symbolic_constraints = get_symbolic_constraints::<Val<SC>, A>(air, 0, public_values.len());
    let constraint_count = symbolic_constraints.len();
    let constraint_degree = symbolic_constraints
        .iter()
        .map(SymbolicExpression::degree_multiple)
        .max()
        .unwrap_or(0);

    // From the degree of the constraint polynomial, compute the number
    // of quotient polynomials we will split Q(x) into. This is chosen to
    // always be a power of 2.
    let log_quotient_degree = log2_ceil_usize(constraint_degree - 1);
    let quotient_degree = 1 << log_quotient_degree;

    // Initialize the PCS and the Challenger.
    let pcs = config.pcs();
    let mut challenger = config.initialise_challenger();

    // Get the subgroup `H` of size `N`. We treat each column `Ti` of
    // the trace as an evaluation vector of polynomials `Ti(x)` over `H`.
    // (In the Circle STARK case `H` is instead a standard position twin coset of size `N`)
    let initial_trace_domain = pcs.natural_domain_for_degree(degree);

    // Let `g` denote a generator of the multiplicative group of `F` and `H'` the unique
    // subgroup of `F` of size `N << pcs.config.log_blowup`.

    // For each trace column `Ti`, we compute the evaluation vector of `Ti(x)` over `gH'`. This
    // new extended trace `ET` is hashed into Merkle tree with it's rows bit-reversed.
    //      trace_commit contains the root of the tree
    //      trace_data contains the entire tree.
    //          - trace_data.leaves is the matrix containing `ET`.
    // TODO: Should this also return the domain `gH'`?
    let (trace_commit, trace_data) = info_span!("commit to trace data")
        .in_scope(|| pcs.commit(vec![(initial_trace_domain, trace)]));

    // Observe the instance.
    // degree < 2^255 so we can safely cast log_degree to a u8.
    challenger.observe(Val::<SC>::from_u8(log_degree as u8));
    // TODO: Might be best practice to include other instance data here; see verifier comment.

    challenger.observe(trace_commit.clone());
    challenger.observe_slice(public_values);

    // FIRST FIAT-SHAMIR CHALLENGE: Anything involved in the proof setup should be included by this point.

    // Get the first Fiat-Shamir challenge, `alpha`, which is used to combine the constraint polynomials.
    let alpha: SC::Challenge = challenger.sample_algebra_element();

    // A domain large enough to uniquely identify the quotient polynomial.
    // This domain must be contained in the domain over which `trace_data` is defined.
    // Explicitly it should be equal to `gK` for some subgroup `K` contained in `H'`.
    let quotient_domain =
        initial_trace_domain.create_disjoint_domain(1 << (log_degree + log_quotient_degree));

    // Return a the subset of the extended trace `ET` corresponding to the rows giving evaluations
    // over the quotient domain.
    //
    // This only works if the trace domain is `gH'` and the quotient domain is `gK` for some subgroup `K` contained in `H'`.
    // TODO: Make this explicit in `get_evaluations_on_domain` or otherwise fix this.
    let trace_on_quotient_domain = pcs.get_evaluations_on_domain(&trace_data, 0, quotient_domain);

    // Compute the quotient polynomial `Q(x)` by evaluating `C(T1(x), ..., Tl(x), T1(gx), ..., Tl(gx), selectors(x)) / Z_H(x)`
    // at every point in the quotient domain. The degree of `Q(x)` is `<= deg(C) * (N - 1) - N + 1 = 2(N - 1)`.
    // The `-N` comes from dividing by `Z_H(x)` and the `+1` is due to the `is_transition` selector.
    let quotient_values = quotient_values(
        air,
        public_values,
        initial_trace_domain,
        quotient_domain,
        trace_on_quotient_domain,
        alpha,
        constraint_count,
    );

    // Due to `alpha`, evaluations of Q all lie in the extension field `G`.
    // We flatten this into a matrix of `F` values by treating `G` as an `F`
    // vector space and so separating each element of `G` into `d = [G: F]` elements of `F`.
    //
    // This is valid to do because our domain lies in the base field `F`. Hence we can split
    // `Q(x)` into `d` polynomials `Q_0(x), ... , Q_{d-1}(x)` each contained in `F`.
    // such that `Q(x) = [Q_0(x), ... ,Q_{d-1}(x)]` holds for all `x` in `F`.
    let quotient_flat = RowMajorMatrix::new_col(quotient_values).flatten_to_base();

    // Currently each polynomial `Q_i(x)` is of degree `<= 2(N - 1)` and
    // we have it's evaluations over a the coset `gK of size `2N`. Let `k` be the chosen
    // generator of `K` which satisfies `k^2 = h`.
    //
    // We can split this coset into the sub-cosets `gH` and `gkH` each of size `N`.
    // Define `L_g(x) = (x^N - (gk)^N)/(g^N - (gk)^N)` and `L_{gk}(x) = (x^N - g^N)/((gk)^N - g^N)`.
    // Then `L_g` is equal to `1` on `gH` and `0` on `gkH` and `L_{gk}` is equal to `1` on `gkH` and `0` on `gH`.
    //
    // Thus we can decompose `Q_i(x) = L_{g}(x)q_{i0}(x) + L_{gk}(x)q_{i1}(x)`
    // where `q_{i0}(x)` and `q_{i1}(x)` are polynomials of degree `<= N - 1`.
    // Moreover the evaluations of `q_{i0}(x), q_{i1}(x)` on `gH` and `gkH` respectively are
    // exactly the evaluations of `Q_i(x)` on `gH` and `gkH`. Hence we can get these evaluation
    // vectors by simply splitting the evaluations of `Q_i(x)` into two halves.
    // quotient_chunks contains the evaluations of `q_{i0}(x)` and `q_{i1}(x)`.
    let quotient_chunks = quotient_domain.split_evals(quotient_degree, quotient_flat);
    let qc_domains = quotient_domain.split_domains(quotient_degree);

    // TODO: This treats the all `q_ij` as if they are evaluations over the domain `H`.
    // This doesn't matter for low degree-ness but we need to be careful when checking
    // equalities.

    // For each polynomial `q_ij`, compute the evaluation vector of `q_ij(x)` over `gH'`. This
    // is then hashed into a Merkle tree with it's rows bit-reversed.
    //      quotient_commit contains the root of the tree
    //      quotient_data contains the entire tree.
    //          - quotient_data.leaves is a pair of matrices containing the `q_i0(x)` and `q_i1(x)`.
    let (quotient_commit, quotient_data) = info_span!("commit to quotient poly chunks")
        .in_scope(|| pcs.commit(izip!(qc_domains, quotient_chunks).collect_vec()));
    challenger.observe(quotient_commit.clone());

    // Combine our commitments to the trace and quotient polynomials into a single object.
    let commitments = Commitments {
        trace: trace_commit,
        quotient_chunks: quotient_commit,
    };

    let zeta: SC::Challenge = challenger.sample();
    let zeta_next = initial_trace_domain.next_point(zeta).unwrap();

    let (opened_values, opening_proof) = info_span!("open").in_scope(|| {
        pcs.open(
            vec![
                (&trace_data, vec![vec![zeta, zeta_next]]),
                (
                    &quotient_data,
                    // open every chunk at zeta
                    (0..quotient_degree).map(|_| vec![zeta]).collect_vec(),
                ),
            ],
            &mut challenger,
        )
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
    let mut sels = debug_span!("Compute Selectors")
        .in_scope(|| trace_domain.selectors_on_coset(quotient_domain));

    let qdb = log2_strict_usize(quotient_domain.size()) - log2_strict_usize(trace_domain.size());
    let next_step = 1 << qdb;

    // We take PackedVal::<SC>::WIDTH worth of values at a time from a quotient_size slice, so we need to
    // pad with default values in the case where quotient_size is smaller than PackedVal::<SC>::WIDTH.
    for _ in quotient_size..PackedVal::<SC>::WIDTH {
        sels.is_first_row.push(Val::<SC>::default());
        sels.is_last_row.push(Val::<SC>::default());
        sels.is_transition.push(Val::<SC>::default());
        sels.inv_vanishing.push(Val::<SC>::default());
    }

    let mut alpha_powers = alpha.powers().take(constraint_count).collect_vec();
    alpha_powers.reverse();
    // alpha powers looks like Vec<EF> ~ Vec<[F; D]>
    // It's useful to also have access to the the transpose of this of form [Vec<F>; D].
    let decomposed_alpha_powers: Vec<_> = (0..SC::Challenge::DIMENSION)
        .map(|i| {
            alpha_powers
                .iter()
                .map(|x| x.as_basis_coefficients_slice()[i])
                .collect()
        })
        .collect();

    (0..quotient_size)
        .into_par_iter()
        .step_by(PackedVal::<SC>::WIDTH)
        .flat_map_iter(|i_start| {
            let i_range = i_start..i_start + PackedVal::<SC>::WIDTH;

            let is_first_row = *PackedVal::<SC>::from_slice(&sels.is_first_row[i_range.clone()]);
            let is_last_row = *PackedVal::<SC>::from_slice(&sels.is_last_row[i_range.clone()]);
            let is_transition = *PackedVal::<SC>::from_slice(&sels.is_transition[i_range.clone()]);
            let inv_vanishing = *PackedVal::<SC>::from_slice(&sels.inv_vanishing[i_range]);

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
                decomposed_alpha_powers: &decomposed_alpha_powers,
                accumulator,
                constraint_index: 0,
            };
            air.eval(&mut folder);

            // quotient(x) = constraints(x) / Z_H(x)
            let quotient = folder.accumulator * inv_vanishing;

            // "Transpose" D packed base coefficients into WIDTH scalar extension coefficients.
            (0..core::cmp::min(quotient_size, PackedVal::<SC>::WIDTH)).map(move |idx_in_packing| {
                SC::Challenge::from_basis_coefficients_fn(|coeff_idx| {
                    quotient.as_basis_coefficients_slice()[coeff_idx].as_slice()[idx_in_packing]
                })
            })
        })
        .collect()
}
