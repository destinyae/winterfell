// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

use super::{constraints::CompositionPoly, StarkDomain, TracePolyTable};
use common::{CompositionCoefficients, ComputationContext, EvaluationFrame};
use math::{
    fft,
    field::{FieldElement, StarkField},
    polynom,
    utils::{self, add_in_place},
};

#[cfg(feature = "concurrent")]
use rayon::prelude::*;

// DEEP COMPOSITION POLYNOMIAL
// ================================================================================================
pub struct DeepCompositionPoly<E: FieldElement> {
    coefficients: Vec<E>,
    cc: CompositionCoefficients<E>,
    z: E,
    field_extension: bool,
}

impl<E: FieldElement> DeepCompositionPoly<E> {
    // CONSTRUCTOR
    // --------------------------------------------------------------------------------------------
    /// Returns a new DEEP composition polynomial. Initially, this polynomial will be empty, and
    /// the intent is to populate the coefficients via add_trace_polys() and add_constraint_polys()
    /// methods.
    pub fn new(context: &ComputationContext, z: E, cc: CompositionCoefficients<E>) -> Self {
        // TODO: change from context to AIR
        DeepCompositionPoly {
            coefficients: vec![],
            cc,
            z,
            field_extension: !context.options().field_extension().is_none(),
        }
    }

    // ACCESSORS
    // --------------------------------------------------------------------------------------------

    /// Returns the size of the DEEP composition polynomial.
    pub fn poly_size(&self) -> usize {
        self.coefficients.len()
    }

    /// Returns the degree of the composition polynomial.
    pub fn degree(&self) -> usize {
        polynom::degree_of(&self.coefficients)
    }

    // TRACE POLYNOMIAL COMPOSITION
    // --------------------------------------------------------------------------------------------
    /// Combines all trace polynomials into a single polynomial and saves the result into
    /// the DEEP composition polynomial. The combination is done as follows:
    ///
    /// - First, state of trace registers at deep points z and z * g are computed.
    /// - Then, polynomials T'_i(x) = (T_i(x) - T_i(z)) / (x - z) and
    ///   T''_i(x) = (T_i(x) - T_i(z * g)) / (x - z * g) are computed for all i, where T_i(x) is
    ///   a trace polynomial for register i.
    /// - Then, all polynomials are combined together using random liner combination as
    ///   T(x) = sum(T'_i(x) * cc'_i + T''_i(x) * cc''_i) for all i, where cc'_i and cc''_i are
    ///   the coefficients for the random linear combination drawn from the public coin.
    /// - In cases when we generate a proof using an extension field, we also compute
    ///   T'''_i(x) = (T_i(x) - T_i(z_conjugate)) / (x - z_conjugate), and add it to T(x) similarly
    ///   to the way described above. This is needed in order to verify that the trace is defined
    ///   over the base field, rather than the extension field.
    pub fn add_trace_polys<B>(&mut self, trace_polys: TracePolyTable<B>) -> EvaluationFrame<E>
    where
        B: StarkField,
        E: From<B>,
    {
        assert!(self.coefficients.is_empty());

        // compute a second out-of-domain point offset from z by exactly trace generator; this
        // point defines the "next" computation state in relation to point z
        let trace_length = trace_polys.poly_size();
        let g = E::from(B::get_root_of_unity(utils::log2(trace_length)));
        let next_z = self.z * g;

        // compute state of registers at points z and z * g
        let trace_state1 = trace_polys.evaluate_at(self.z);
        let trace_state2 = trace_polys.evaluate_at(next_z);

        // combine trace polynomials into 2 composition polynomials T'(x) and T''(x), and if
        // we are using a field extension, also T'''(x)
        let polys = trace_polys.into_vec();
        let mut t1_composition = E::zeroed_vector(trace_length);
        let mut t2_composition = E::zeroed_vector(trace_length);
        let mut t3_composition = if self.field_extension {
            E::zeroed_vector(trace_length)
        } else {
            Vec::new()
        };
        for (i, poly) in polys.into_iter().enumerate() {
            // compute T'(x) = T(x) - T(z), multiply it by a pseudo-random coefficient,
            // and add the result into composition polynomial
            acc_poly(
                &mut t1_composition,
                &poly,
                trace_state1[i],
                self.cc.trace[i].0,
            );

            // compute T''(x) = T(x) - T(z * g), multiply it by a pseudo-random coefficient,
            // and add the result into composition polynomial
            acc_poly(
                &mut t2_composition,
                &poly,
                trace_state2[i],
                self.cc.trace[i].1,
            );

            // when extension field is enabled, compute T'''(x) = T(x) - T(z_conjugate), multiply
            // it by a pseudo-random coefficient, and add the result into composition polynomial
            if self.field_extension {
                acc_poly(
                    &mut t3_composition,
                    &poly,
                    trace_state1[i].conjugate(),
                    self.cc.trace[i].2,
                );
            }
        }

        // divide the composition polynomials by (x - z), (x - z * g), and (x - z_conjugate)
        // respectively, and add the resulting polynomials together; the output of this step
        // is a single trace polynomial T(x) and deg(T(x)) = trace_length - 2.
        let trace_poly = merge_trace_compositions(
            vec![t1_composition, t2_composition, t3_composition],
            vec![self.z, next_z, self.z.conjugate()],
        );

        // set the coefficients of the DEEP composition polynomial
        self.coefficients = trace_poly;
        assert_eq!(self.poly_size() - 2, self.degree());

        // trace states at OOD points z and z * g are returned to be included in the proof
        EvaluationFrame {
            current: trace_state1,
            next: trace_state2,
        }
    }

    // CONSTRAINT POLYNOMIAL COMPOSITION
    // --------------------------------------------------------------------------------------------
    /// Divides out OOD point z from the constraint composition polynomial and saves the result
    /// into the DEEP composition polynomial. This method is intended to be called only after the
    /// add_trace_polys() method has been executed. The composition is done as follows:
    ///
    /// - For each H_i(x), compute H'_i(x) = (H_i(x) - H(z^m)) / (x - z^m), where H_i(x) is the
    ///   ith composition polynomial column and m is the total number of columns.
    /// - Then, combine all H_i(x) polynomials together by computing H(x) = sum(H_i(x) * cc_i) for
    ///   all i, where cc_i is the coefficient for the random linear combination drawn from the
    ///   public coin.
    ///
    /// This method returns evaluations of the column polynomials H_i(x) at z^m.
    pub fn add_composition_poly(&mut self, composition_poly: CompositionPoly<E>) -> Vec<E> {
        assert!(!self.coefficients.is_empty());

        // compute z^m
        let num_columns = composition_poly.num_columns() as u32;
        let z_m = self.z.exp(num_columns.into());

        let mut column_polys = composition_poly.into_columns();

        // Divide out the OOD point z from column polynomials
        #[cfg(not(feature = "concurrent"))]
        let result = column_polys
            .iter_mut()
            .map(|poly| {
                // evaluate the polynomial at point z^m
                let value_at_z_m = polynom::eval(&poly, z_m);

                // compute H'_i(x) = (H_i(x) - H_i(z^m)) / (x - z^m)
                poly[0] -= value_at_z_m;
                polynom::syn_div_in_place(poly, 1, z_m);

                value_at_z_m
            })
            .collect();

        #[cfg(feature = "concurrent")]
        let result = column_polys
            .par_iter_mut()
            .map(|poly| {
                // evaluate the polynomial at point z'
                let value_at_z = polynom::eval(&poly, z_m);

                // compute C(x) = (P(x) - P(z)) / (x - z')
                poly[0] -= value_at_z;
                polynom::syn_div_in_place(poly, 1, z_m);

                value_at_z
            })
            .collect();

        // add H'_i(x) * cc_i for all i into the DEEP composition polynomial
        for (i, poly) in column_polys.into_iter().enumerate() {
            utils::mul_acc(&mut self.coefficients, &poly, self.cc.constraints[i]);
        }
        assert_eq!(self.poly_size() - 2, self.degree());

        result
    }

    // FINAL DEGREE ADJUSTMENT
    // --------------------------------------------------------------------------------------------
    /// Increase the degree of the DEEP composition polynomial by one. After add_trace_polys() and
    /// add_composition_poly() are executed, the degree of the DEEP composition polynomial is
    /// trace_length - 2 because in these functions we divide the polynomials of degree
    /// trace_length - 1 by (x - z), (x - z * g) etc. which decreases the degree by one. We want to
    /// ensure that degree of the DEEP composition polynomial is trace_length - 1, so we make the
    /// adjustment here by computing C'(x) = C(x) * (cc_0 + x * cc_1), where cc_0 and cc_1 are the
    /// coefficients for the random linear combination drawn from the public coin.
    pub fn adjust_degree(&mut self) {
        assert_eq!(self.poly_size() - 2, self.degree());

        let mut result = E::zeroed_vector(self.coefficients.len());

        // this is equivalent to C(x) * cc_0
        utils::mul_acc(&mut result, &self.coefficients, self.cc.degree.0);
        // this is equivalent to C(x) * x * cc_1
        utils::mul_acc(
            &mut result[1..],
            &self.coefficients[..(self.coefficients.len() - 1)],
            self.cc.degree.1,
        );

        self.coefficients = result;
        assert_eq!(self.poly_size() - 1, self.degree());
    }

    // LOW-DEGREE EXTENSION
    // --------------------------------------------------------------------------------------------
    /// Evaluates DEEP composition polynomial over the specified LDE domain and returns the result.
    pub fn evaluate<B>(self, domain: &StarkDomain<B>) -> Vec<E>
    where
        B: StarkField,
        E: From<B>,
    {
        fft::evaluate_poly_with_offset(
            &self.coefficients,
            domain.trace_twiddles(),
            domain.offset(),
            domain.trace_to_lde_blowup(),
        )
    }
}

// HELPER FUNCTIONS
// ================================================================================================

/// Divides each polynomial in the list by the corresponding divisor, and computes the
/// coefficient-wise sum of all resulting polynomials.
#[cfg(not(feature = "concurrent"))]
fn merge_trace_compositions<E: FieldElement>(mut polys: Vec<Vec<E>>, divisors: Vec<E>) -> Vec<E> {
    // divide all polynomials by their corresponding divisor
    for (poly, &divisor) in polys.iter_mut().zip(divisors.iter()) {
        // skip empty polynomials; this could happen for conjugate composition polynomial (T3)
        // when extension field is not enabled.
        if !poly.is_empty() {
            polynom::syn_div_in_place(poly, 1, divisor);
        }
    }

    // add all polynomials together into a single polynomial
    let mut result = polys.remove(0);
    for poly in polys.iter() {
        if !poly.is_empty() {
            add_in_place(&mut result, poly);
        }
    }

    result
}

/// Same as above function, but performs division in parallel threads.
#[cfg(feature = "concurrent")]
fn merge_trace_compositions<E: FieldElement>(mut polys: Vec<Vec<E>>, divisors: Vec<E>) -> Vec<E> {
    polys
        .par_iter_mut()
        .zip(divisors.par_iter())
        .for_each(|(poly, &divisor)| {
            if !poly.is_empty() {
                polynom::syn_div_in_place(poly, 1, divisor);
            }
        });

    let mut result = polys.remove(0);
    for poly in polys.iter() {
        if !poly.is_empty() {
            add_in_place(&mut result, poly);
        }
    }

    result
}

/// Computes (P(x) - value) * k and saves the result into the accumulator
fn acc_poly<B, E>(accumulator: &mut Vec<E>, poly: &[B], value: E, k: E)
where
    B: StarkField,
    E: FieldElement + From<B>,
{
    utils::mul_acc(accumulator, poly, k);
    let adjusted_tz = value * k;
    accumulator[0] -= adjusted_tz;
}