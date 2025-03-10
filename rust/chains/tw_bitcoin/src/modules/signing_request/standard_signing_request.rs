// SPDX-License-Identifier: Apache-2.0
//
// Copyright © 2017 Trust Wallet.

use crate::context::StandardBitcoinContext;
use crate::modules::signing_request::SigningRequestBuilder;
use crate::modules::tx_builder::output_protobuf::OutputProtobuf;
use crate::modules::tx_builder::public_keys::PublicKeys;
use crate::modules::tx_builder::utxo_protobuf::UtxoProtobuf;
use crate::modules::tx_builder::BitcoinChainInfo;
use tw_coin_entry::coin_context::CoinContext;
use tw_coin_entry::error::prelude::*;
use tw_misc::traits::OptionalEmpty;
use tw_proto::BitcoinV2::Proto;
use tw_utxo::context::UtxoContext;
use tw_utxo::dust::DustPolicy;
use tw_utxo::fee::fee_estimator::StandardFeeEstimator;
use tw_utxo::fee::FeePolicy;
use tw_utxo::modules::tx_planner::{PlanRequest, RequestType};
use tw_utxo::modules::utxo_selector::InputSelector;
use tw_utxo::transaction::standard_transaction::builder::TransactionBuilder;
use tw_utxo::transaction::standard_transaction::Transaction;
use Proto::mod_TransactionBuilder::OneOfdust_policy as ProtoDustPolicy;

const DEFAULT_TX_VERSION: u32 = 1;

pub type StandardSigningRequest = PlanRequest<StandardBitcoinContext>;

pub struct StandardSigningRequestBuilder;

impl<Context> SigningRequestBuilder<Context> for StandardSigningRequestBuilder
where
    Context:
        UtxoContext<Transaction = Transaction, FeeEstimator = StandardFeeEstimator<Transaction>>,
{
    fn build(
        coin: &dyn CoinContext,
        input: &Proto::SigningInput,
        transaction_builder: &Proto::TransactionBuilder,
    ) -> SigningResult<PlanRequest<Context>> {
        let chain_info = chain_info(coin, &input.chain_info)?;
        let dust_policy = Self::dust_policy(&transaction_builder.dust_policy)?;
        let fee_estimator = Self::fee_estimator(transaction_builder)?;
        let version = Self::transaction_version(&transaction_builder.version, DEFAULT_TX_VERSION);

        let public_keys = Self::get_public_keys::<Context>(input)?;

        let mut builder = TransactionBuilder::default();
        builder
            .version(version)
            .lock_time(transaction_builder.lock_time);

        // Parse all UTXOs.
        for utxo_proto in transaction_builder.inputs.iter() {
            let utxo_builder = UtxoProtobuf::<Context>::new(&chain_info, utxo_proto, &public_keys);

            let (utxo, utxo_args) = utxo_builder
                .utxo_from_proto()
                .context("Error creating UTXO from Protobuf")?;
            builder.push_input(utxo, utxo_args);
        }

        // If `max_amount_output` is set, construct a transaction with only one output.
        if let Some(max_output_proto) = transaction_builder.max_amount_output.as_ref() {
            let output_builder = OutputProtobuf::<Context>::new(&chain_info, max_output_proto);

            let max_output = output_builder
                .output_from_proto()
                .context("Error creating Max Output from Protobuf")?;
            builder.push_output(max_output);

            let unsigned_tx = builder.build()?;
            return Ok(PlanRequest {
                ty: RequestType::SendMax { unsigned_tx },
                dust_policy,
                fee_estimator,
            });
        }

        // `max_amount_output` isn't set, parse all Outputs.
        for output_proto in transaction_builder.outputs.iter() {
            let output = OutputProtobuf::<Context>::new(&chain_info, output_proto)
                .output_from_proto()
                .context("Error creating Output from Proto")?;
            builder.push_output(output);
        }

        // Parse change output if it was provided.
        let change_output = transaction_builder
            .change_output
            .as_ref()
            .map(|change_output_proto| {
                OutputProtobuf::<Context>::new(&chain_info, change_output_proto)
                    .output_from_proto()
                    .context("Error creating Change Output from Proto")
            })
            .transpose()?;

        let input_selector = Self::input_selector(&transaction_builder.input_selector);

        let unsigned_tx = builder.build()?;
        Ok(PlanRequest {
            ty: RequestType::SendExact {
                unsigned_tx,
                change_output,
                input_selector,
            },
            dust_policy,
            fee_estimator,
        })
    }
}

impl StandardSigningRequestBuilder {
    pub fn get_public_keys<Context: UtxoContext>(
        input: &Proto::SigningInput,
    ) -> SigningResult<PublicKeys> {
        let mut public_keys = PublicKeys::with_public_key_hasher(Context::PUBLIC_KEY_HASHER);

        if input.private_keys.is_empty() {
            for public in input.public_keys.iter() {
                public_keys.add_public_key(public.to_vec())?;
            }
        } else {
            for private in input.private_keys.iter() {
                public_keys.add_public_with_ecdsa_private(private)?;
            }
        }

        Ok(public_keys)
    }

    pub fn input_selector(selector: &Proto::InputSelector) -> InputSelector {
        match selector {
            Proto::InputSelector::SelectAscending => InputSelector::Ascending,
            Proto::InputSelector::SelectInOrder => InputSelector::InOrder,
            Proto::InputSelector::SelectDescending => InputSelector::Descending,
            Proto::InputSelector::UseAll => InputSelector::UseAll,
        }
    }

    pub fn dust_policy(proto: &ProtoDustPolicy) -> SigningResult<DustPolicy> {
        match proto {
            ProtoDustPolicy::fixed_dust_threshold(fixed) => Ok(DustPolicy::FixedAmount(*fixed)),
            ProtoDustPolicy::None => SigningError::err(SigningErrorType::Error_invalid_params)
                .context("No dust policy provided"),
        }
    }

    pub fn fee_estimator<Transaction>(
        proto: &Proto::TransactionBuilder,
    ) -> SigningResult<StandardFeeEstimator<Transaction>> {
        let fee_policy = FeePolicy::FeePerVb(proto.fee_per_vb);
        Ok(StandardFeeEstimator::new(fee_policy))
    }

    pub fn transaction_version(proto: &Proto::TransactionVersion, default: u32) -> u32 {
        match proto {
            Proto::TransactionVersion::UseDefault => default,
            Proto::TransactionVersion::V1 => 1,
            Proto::TransactionVersion::V2 => 2,
        }
    }

    pub fn expect_transaction_version(
        proto: &Proto::TransactionVersion,
        expected: u32,
    ) -> SigningResult<u32> {
        if Self::transaction_version(proto, expected) != expected {
            return SigningError::err(SigningErrorType::Error_invalid_params).context(format!(
                "Invalid transaction 'version'. Expected Default or V{expected}"
            ));
        }
        Ok(expected)
    }
}

pub fn chain_info(
    coin: &dyn CoinContext,
    chain_info: &Option<Proto::ChainInfo>,
) -> SigningResult<BitcoinChainInfo> {
    fn prefix_to_u8(prefix: u32, prefix_name: &str) -> SigningResult<u8> {
        prefix
            .try_into()
            .tw_err(SigningErrorType::Error_invalid_params)
            .with_context(|| format!("Invalid {prefix_name} prefix. It must fit uint8"))
    }

    if let Some(info) = chain_info {
        let hrp = info.hrp.to_string().empty_or_some();
        return Ok(BitcoinChainInfo {
            p2pkh_prefix: prefix_to_u8(info.p2pkh_prefix, "p2pkh")?,
            p2sh_prefix: prefix_to_u8(info.p2sh_prefix, "p2sh")?,
            hrp,
        });
    }

    // Try to get the chain info from the context.
    // Note that not all Bitcoin forks support HRP (segwit addresses).
    let hrp = coin.hrp();
    match (coin.p2pkh_prefix(), coin.p2sh_prefix()) {
        (Some(p2pkh_prefix), Some(p2sh_prefix)) => Ok(BitcoinChainInfo {
            p2pkh_prefix,
            p2sh_prefix,
            hrp,
        }),
        _ => SigningError::err(SigningErrorType::Error_invalid_params)
            .context("Neither 'SigningInput.chain_info' nor p2pkh/p2sh prefixes specified in the registry.json")
    }
}
