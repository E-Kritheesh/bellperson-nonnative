use byteorder::LittleEndian;
use byteorder::WriteBytesExt;

use rand::Rng;

use sapling_crypto::bellman::pairing::ff::{PrimeField, PrimeFieldRepr};
use sapling_crypto::eddsa::{PrivateKey, PublicKey, Signature};
use sapling_crypto::jubjub::{FixedGenerators, JubjubEngine};

use hash::tree::Hasher;

use usize_to_f;

const MAX_SIGNED_MESSAGE_SIZE: usize = 8 + 8 + 32 * 2;

#[derive(Derivative)]
#[derivative(Clone(bound = ""))]
pub struct Action<E: JubjubEngine> {
    pub dst: PublicKey<E>,
    pub amt: u64,
    pub tx_no: u64,
}

impl<E: JubjubEngine> Action<E> {
    pub fn as_elems(&self) -> Vec<E::Fr> {
        vec![
            self.dst.0.into_xy().0.clone(),
            self.dst.0.into_xy().1.clone(),
            usize_to_f(self.amt as usize),
            usize_to_f(self.tx_no as usize),
        ]
    }
    pub fn sign<R: Rng, H: Hasher<F = E::Fr>>(
        &self,
        rng: &mut R,
        p_g: FixedGenerators,
        params: &E::Params,
        hasher: &H,
        sk: &PrivateKey<E>,
    ) -> SignedTx<E> {
        let hash = hasher.hash_chain(&self.as_elems());
        let mut bytes = Vec::new();
        hash.into_repr().write_le(&mut bytes).unwrap();
        bytes.truncate((E::Fr::CAPACITY / 8) as usize);
        SignedTx {
            tx: Tx {
                src: PublicKey::from_private(sk, p_g, params),
                action: self.clone(),
            },
            sig: sk.sign_raw_message(&bytes, rng, p_g, params, (E::Fr::CAPACITY / 8) as usize),
        }
    }
}

#[derive(Derivative)]
#[derivative(Clone(bound = ""))]
pub struct Tx<E: JubjubEngine> {
    pub src: PublicKey<E>,
    pub action: Action<E>,
}

#[derive(Derivative)]
#[derivative(Clone(bound = ""))]
pub struct SignedTx<E: JubjubEngine> {
    pub tx: Tx<E>,
    pub sig: Signature<E>,
}

pub mod circuit {
    use super::{Action, SignedTx, MAX_SIGNED_MESSAGE_SIZE};
    use gadget::Gadget;
    use hash::tree::circuit::CircuitHasher;
    use rollup::sig::allocate_sig;
    use sapling_crypto::bellman::pairing::ff::ScalarEngine;
    use sapling_crypto::bellman::{ConstraintSystem, LinearCombination, SynthesisError};
    use sapling_crypto::bellman::pairing::ff::PrimeField;
    use sapling_crypto::circuit::baby_eddsa::EddsaSignature;
    use sapling_crypto::circuit::boolean::Boolean;
    use sapling_crypto::circuit::ecc::EdwardsPoint;
    use sapling_crypto::circuit::num::AllocatedNum;
    use sapling_crypto::jubjub::JubjubEngine;
    use std::clone::Clone;
    use std::rc::Rc;
    use usize_to_f;
    use CResult;
    use OptionExt;

    #[derive(Derivative)]
    #[derivative(Clone(bound = ""))]
    pub struct CircuitAction<E: JubjubEngine> {
        pub dst: EdwardsPoint<E>,
        pub amt: AllocatedNum<E>,
        pub tx_no: AllocatedNum<E>,
        pub params: Rc<E::Params>,
        pub value: Option<Action<E>>,
    }

    impl<E: JubjubEngine> Gadget for CircuitAction<E> {
        type E = E;
        type Value = Action<E>;
        type Access = ();
        type Params = Rc<E::Params>;
        fn alloc<CS: ConstraintSystem<Self::E>>(
            mut cs: CS,
            value: Option<&Self::Value>,
            _access: Self::Access,
            params: &Self::Params,
        ) -> Result<Self, SynthesisError> {
            let dst_x = AllocatedNum::alloc(cs.namespace(|| "dst_x"), || {
                Ok(value.grab()?.dst.0.into_xy().0)
            })?;
            let dst_y = AllocatedNum::alloc(cs.namespace(|| "dst_y"), || {
                Ok(value.grab()?.dst.0.into_xy().1)
            })?;
            let amt = AllocatedNum::alloc(cs.namespace(|| "amt"), || {
                Ok(usize_to_f(value.grab()?.amt as usize))
            })?;
            let tx_no = AllocatedNum::alloc(cs.namespace(|| "tx_no"), || {
                Ok(usize_to_f(value.grab()?.tx_no as usize))
            })?;
            Ok(Self {
                dst: EdwardsPoint::interpret(cs.namespace(|| "dst"), &dst_x, &dst_y, &params)?,
                amt,
                tx_no,
                params: params.clone(),
                value: value.cloned(),
            })
        }
        fn wires(&self) -> Vec<LinearCombination<Self::E>> {
            vec![
                LinearCombination::zero() + self.dst.get_x().get_variable(),
                LinearCombination::zero() + self.dst.get_y().get_variable(),
                LinearCombination::zero() + self.amt.get_variable(),
                LinearCombination::zero() + self.tx_no.get_variable(),
            ]
        }
        fn wire_values(&self) -> Option<Vec<<Self::E as ScalarEngine>::Fr>> {
            vec![
                self.dst.get_x().get_value(),
                self.dst.get_y().get_value(),
                self.amt.get_value(),
                self.tx_no.get_value(),
            ]
            .into_iter()
            .collect::<Option<Vec<_>>>()
        }
        fn value(&self) -> Option<&Self::Value> {
            self.value.as_ref()
        }
        fn access(&self) -> &Self::Access {
            &()
        }
        fn params(&self) -> &Self::Params {
            &self.params
        }
    }

    impl<E: JubjubEngine> CircuitAction<E> {
        pub fn as_elems(&self) -> Vec<AllocatedNum<E>> {
            vec![
                self.dst.get_x().clone(),
                self.dst.get_y().clone(),
                self.amt.clone(),
                self.tx_no.clone(),
            ]
        }
        pub fn check_signature<CS: ConstraintSystem<E>, H: CircuitHasher<E = E>>(
            &self,
            mut cs: CS,
            generator: EdwardsPoint<E>,
            hasher: &H,
            signature: EddsaSignature<E>,
        ) -> CResult<()> {
            let elems = self.as_elems();
            let hash = hasher.allocate_hash_chain(cs.namespace(|| "hash"), &elems)?;
            let mut bits = hash.into_bits_le_strict(cs.namespace(|| "bits"))?;
            bits.truncate((E::Fr::CAPACITY / 8 * 8) as usize);
            signature.verify_raw_message_signature(
                cs.namespace(|| "check sig"),
                &self.params,
                &bits,
                generator,
                (E::Fr::CAPACITY / 8) as usize,
            )
        }
    }

    #[derive(Derivative)]
    #[derivative(Clone(bound = ""))]
    pub struct CircuitSignedTx<E: JubjubEngine> {
        pub action: CircuitAction<E>,
        pub src: EdwardsPoint<E>,
        pub sig: EddsaSignature<E>,
        pub value: Option<SignedTx<E>>,
        pub params: Rc<E::Params>,
    }

    impl<E: JubjubEngine> Gadget for CircuitSignedTx<E> {
        type E = E;
        type Value = SignedTx<E>;
        type Access = ();
        type Params = Rc<E::Params>;
        fn alloc<CS: ConstraintSystem<Self::E>>(
            mut cs: CS,
            value: Option<&Self::Value>,
            _access: Self::Access,
            params: &Self::Params,
        ) -> Result<Self, SynthesisError> {
            let action = CircuitAction::alloc(
                cs.namespace(|| "action"),
                value.as_ref().map(|v| &v.tx.action),
                (),
                params,
            )?;
            let sig = allocate_sig(
                cs.namespace(|| "src"),
                value.as_ref().map(|v| (v.sig.clone(), v.tx.src.clone())),
                params.as_ref(),
            )?;
            Ok(Self {
                src: sig.pk.clone(),
                action,
                sig,
                value: value.cloned(),
                params: params.clone(),
            })
        }
        fn wires(&self) -> Vec<LinearCombination<Self::E>> {
            let mut v = self.action.wires();
            v.push(LinearCombination::zero() + self.sig.pk.get_x().get_variable());
            v.push(LinearCombination::zero() + self.sig.pk.get_y().get_variable());
            v.push(LinearCombination::zero() + self.sig.r.get_x().get_variable());
            v.push(LinearCombination::zero() + self.sig.r.get_y().get_variable());
            v.push(LinearCombination::zero() + self.sig.s.get_variable());
            v
        }
        fn wire_values(&self) -> Option<Vec<<Self::E as ScalarEngine>::Fr>> {
            if let Some(mut v) = self.action.wire_values() {
                v.push(self.sig.pk.get_x().get_value()?);
                v.push(self.sig.pk.get_y().get_value()?);
                v.push(self.sig.r.get_x().get_value()?);
                v.push(self.sig.r.get_y().get_value()?);
                v.push(self.sig.s.get_value()?);
                Some(v)
            } else {
                None
            }
        }
        fn value(&self) -> Option<&Self::Value> {
            self.value.as_ref()
        }
        fn access(&self) -> &Self::Access {
            &()
        }
        fn params(&self) -> &Self::Params {
            &self.params
        }
    }

    impl<E: JubjubEngine> CircuitSignedTx<E> {
        pub fn check_signature<CS: ConstraintSystem<E>, H: CircuitHasher<E = E>>(
            &self,
            cs: CS,
            hasher: &H,
            gen: EdwardsPoint<E>,
        ) -> CResult<()> {
            self.action.check_signature(cs, gen, hasher, self.sig.clone())
        }
    }
}