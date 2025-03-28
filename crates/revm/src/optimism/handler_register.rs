//! Handler related to Optimism chain

use crate::{
    handler::{
        mainnet::{self, deduct_caller_inner},
        register::EvmHandler,
    },
    interpreter::{return_ok, return_revert, Gas, InstructionResult},
    optimism,
    primitives::{
        db::Database, spec_to_generic, Account, EVMError, Env, ExecutionResult, HaltReason,
        HashMap, InvalidTransaction, OptimismInvalidTransaction, ResultAndState, Spec, SpecId,
        SpecId::REGOLITH, U256,
    },
    Context, ContextPrecompiles, FrameResult,
};
use core::{cmp::Ordering, ops::Mul};
use revm_precompile::PrecompileSpecId;
use std::{boxed::Box, string::ToString, sync::Arc};

use super::l1block::OPERATOR_FEE_RECIPIENT;

pub fn optimism_handle_register<DB: Database, EXT>(handler: &mut EvmHandler<'_, EXT, DB>) {
    spec_to_generic!(handler.cfg.spec_id, {
        // validate environment
        handler.validation.env = Arc::new(validate_env::<SPEC, DB>);
        // Validate transaction against state.
        handler.validation.tx_against_state = Arc::new(validate_tx_against_state::<SPEC, EXT, DB>);
        // Load additional precompiles for the given chain spec.
        handler.pre_execution.load_precompiles = Arc::new(load_precompiles::<SPEC, EXT, DB>);
        // An estimated batch cost is charged from the caller and added to L1 Fee Vault.
        handler.pre_execution.deduct_caller = Arc::new(deduct_caller::<SPEC, EXT, DB>);
        // Refund is calculated differently then mainnet.
        handler.execution.last_frame_return = Arc::new(last_frame_return::<SPEC, EXT, DB>);
        handler.post_execution.refund = Arc::new(refund::<SPEC, EXT, DB>);
        handler.post_execution.reimburse_caller = Arc::new(reimburse_caller::<SPEC, EXT, DB>);
        handler.post_execution.reward_beneficiary = Arc::new(reward_beneficiary::<SPEC, EXT, DB>);
        // In case of halt of deposit transaction return Error.
        handler.post_execution.output = Arc::new(output::<SPEC, EXT, DB>);
        handler.post_execution.end = Arc::new(end::<SPEC, EXT, DB>);
        handler.post_execution.clear = Arc::new(clear::<EXT, DB>);
    });
}

/// Validate environment for the Optimism chain.
pub fn validate_env<SPEC: Spec, DB: Database>(env: &Env) -> Result<(), EVMError<DB::Error>> {
    // Do not perform any extra validation for deposit transactions, they are pre-verified on L1.
    if env.tx.optimism.source_hash.is_some() {
        return Ok(());
    }
    // Important: validate block before tx.
    env.validate_block_env::<SPEC>()?;

    // Do not allow for a system transaction to be processed if Regolith is enabled.
    let tx = &env.tx.optimism;
    if tx.is_system_transaction.unwrap_or(false) && SPEC::enabled(SpecId::REGOLITH) {
        return Err(InvalidTransaction::OptimismError(
            OptimismInvalidTransaction::DepositSystemTxPostRegolith,
        )
        .into());
    }

    env.validate_tx::<SPEC>()?;
    Ok(())
}

/// Don not perform any extra validation for deposit transactions, they are pre-verified on L1.
pub fn validate_tx_against_state<SPEC: Spec, EXT, DB: Database>(
    context: &mut Context<EXT, DB>,
) -> Result<(), EVMError<DB::Error>> {
    // No validation is needed for deposit transactions, as they are pre-verified on L1.
    if context.evm.inner.env.tx.optimism.source_hash.is_some() {
        return Ok(());
    }

    // storage l1 block info for later use. l1_block_info is cleared after execution.
    if context.evm.inner.l1_block_info.is_none() {
        // the L1-cost fee is only computed for Optimism non-deposit transactions.
        let l1_block_info =
            crate::optimism::L1BlockInfo::try_fetch(&mut context.evm.inner.db, SPEC::SPEC_ID)
                .map_err(EVMError::Database)?;
        context.evm.inner.l1_block_info = Some(l1_block_info);
    }

    let env @ Env { cfg, tx, .. } = context.evm.inner.env.as_ref();

    // load acc
    let tx_caller = tx.caller;
    let account = context
        .evm
        .inner
        .journaled_state
        .load_code(tx_caller, &mut context.evm.inner.db)?
        .data;

    // EIP-3607: Reject transactions from senders with deployed code
    // This EIP is introduced after london but there was no collision in past
    // so we can leave it enabled always
    if !cfg.is_eip3607_disabled() {
        let bytecode = &account.info.code.as_ref().unwrap();
        // allow EOAs whose code is a valid delegation designation,
        // i.e. 0xef0100 || address, to continue to originate transactions.
        if !bytecode.is_empty() && !bytecode.is_eip7702() {
            return Err(EVMError::Transaction(
                InvalidTransaction::RejectCallerWithCode,
            ));
        }
    }

    // Check that the transaction's nonce is correct
    if let Some(tx) = tx.nonce {
        let state = account.info.nonce;
        match tx.cmp(&state) {
            Ordering::Greater => {
                return Err(EVMError::Transaction(InvalidTransaction::NonceTooHigh {
                    tx,
                    state,
                }));
            }
            Ordering::Less => {
                return Err(EVMError::Transaction(InvalidTransaction::NonceTooLow {
                    tx,
                    state,
                }));
            }
            _ => {}
        }
    }

    // get envelope
    let Some(enveloped_tx) = &tx.optimism.enveloped_tx else {
        return Err(EVMError::Custom(
            "[OPTIMISM] Failed to load enveloped transaction.".to_string(),
        ));
    };

    // compute L1 cost
    let tx_l1_cost = context
        .evm
        .inner
        .l1_block_info
        .as_mut()
        .expect("L1BlockInfo should be loaded")
        .calculate_tx_l1_cost(enveloped_tx, SPEC::SPEC_ID);

    let gas_limit = U256::from(tx.gas_limit);
    let operator_fee_charge = context
        .evm
        .inner
        .l1_block_info
        .as_ref()
        .expect("L1BlockInfo should be loaded")
        .operator_fee_charge(enveloped_tx, gas_limit, SPEC::SPEC_ID);

    let mut balance_check = gas_limit
        .checked_mul(tx.gas_price)
        .and_then(|gas_cost| gas_cost.checked_add(tx.value))
        .and_then(|total_cost| total_cost.checked_add(tx_l1_cost))
        .and_then(|total_cost| total_cost.checked_add(operator_fee_charge))
        .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;

    if SPEC::enabled(SpecId::CANCUN) {
        // if the tx is not a blob tx, this will be None, so we add zero
        let data_fee = env.calc_max_data_fee().unwrap_or_default();
        balance_check = balance_check
            .checked_add(U256::from(data_fee))
            .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;
    }

    // Check if account has enough balance for gas_limit*gas_price and value transfer.
    // Transfer will be done inside `*_inner` functions.
    if balance_check > account.info.balance {
        if cfg.is_balance_check_disabled() {
            // Add transaction cost to balance to ensure execution doesn't fail.
            account.info.balance = balance_check;
        } else {
            return Err(EVMError::Transaction(
                InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(balance_check),
                    balance: Box::new(account.info.balance),
                },
            ));
        }
    }

    Ok(())
}

/// Handle output of the transaction
#[inline]
pub fn last_frame_return<SPEC: Spec, EXT, DB: Database>(
    context: &mut Context<EXT, DB>,
    frame_result: &mut FrameResult,
) -> Result<(), EVMError<DB::Error>> {
    let env = context.evm.inner.env();
    let is_deposit = env.tx.optimism.source_hash.is_some();
    let tx_system = env.tx.optimism.is_system_transaction;
    let tx_gas_limit = env.tx.gas_limit;
    let is_regolith = SPEC::enabled(REGOLITH);

    let instruction_result = frame_result.interpreter_result().result;
    let gas = frame_result.gas_mut();
    let remaining = gas.remaining();
    let refunded = gas.refunded();
    // Spend the gas limit. Gas is reimbursed when the tx returns successfully.
    *gas = Gas::new_spent(tx_gas_limit);

    match instruction_result {
        return_ok!() => {
            // On Optimism, deposit transactions report gas usage uniquely to other
            // transactions due to them being pre-paid on L1.
            //
            // Hardfork Behavior:
            // - Bedrock (success path):
            //   - Deposit transactions (non-system) report their gas limit as the usage.
            //     No refunds.
            //   - Deposit transactions (system) report 0 gas used. No refunds.
            //   - Regular transactions report gas usage as normal.
            // - Regolith (success path):
            //   - Deposit transactions (all) report their gas used as normal. Refunds
            //     enabled.
            //   - Regular transactions report their gas used as normal.
            if !is_deposit || is_regolith {
                // For regular transactions prior to Regolith and all transactions after
                // Regolith, gas is reported as normal.
                gas.erase_cost(remaining);
                gas.record_refund(refunded);
            } else if is_deposit && tx_system.unwrap_or(false) {
                // System transactions were a special type of deposit transaction in
                // the Bedrock hardfork that did not incur any gas costs.
                gas.erase_cost(tx_gas_limit);
            }
        }
        return_revert!() => {
            // On Optimism, deposit transactions report gas usage uniquely to other
            // transactions due to them being pre-paid on L1.
            //
            // Hardfork Behavior:
            // - Bedrock (revert path):
            //   - Deposit transactions (all) report the gas limit as the amount of gas
            //     used on failure. No refunds.
            //   - Regular transactions receive a refund on remaining gas as normal.
            // - Regolith (revert path):
            //   - Deposit transactions (all) report the actual gas used as the amount of
            //     gas used on failure. Refunds on remaining gas enabled.
            //   - Regular transactions receive a refund on remaining gas as normal.
            if !is_deposit || is_regolith {
                gas.erase_cost(remaining);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Record Eip-7702 refund and calculate final refund.
#[inline]
pub fn refund<SPEC: Spec, EXT, DB: Database>(
    context: &mut Context<EXT, DB>,
    gas: &mut Gas,
    eip7702_refund: i64,
) {
    gas.record_refund(eip7702_refund);

    let env = context.evm.inner.env();
    let is_deposit = env.tx.optimism.source_hash.is_some();
    let is_regolith = SPEC::enabled(REGOLITH);

    // Prior to Regolith, deposit transactions did not receive gas refunds.
    let is_gas_refund_disabled = env.cfg.is_gas_refund_disabled() || (is_deposit && !is_regolith);
    if !is_gas_refund_disabled {
        gas.set_final_refund(SPEC::SPEC_ID.is_enabled_in(SpecId::LONDON));
    }
}

/// Reimburse the transaction caller.
#[inline]
pub fn reimburse_caller<SPEC: Spec, EXT, DB: Database>(
    context: &mut Context<EXT, DB>,
    gas: &Gas,
) -> Result<(), EVMError<DB::Error>> {
    mainnet::reimburse_caller::<SPEC, EXT, DB>(context, gas)?;

    if context.evm.inner.env.tx.optimism.source_hash.is_none() {
        let caller_account = context
            .evm
            .inner
            .journaled_state
            .load_account(context.evm.inner.env.tx.caller, &mut context.evm.inner.db)?;
        let operator_fee_refund = context
            .evm
            .inner
            .l1_block_info
            .as_ref()
            .expect("L1BlockInfo should be loaded")
            .operator_fee_refund(gas, SPEC::SPEC_ID);

        // In additional to the normal transaction fee, additionally refund the caller
        // for the operator fee.
        caller_account.data.info.balance = caller_account
            .data
            .info
            .balance
            .saturating_add(operator_fee_refund);
    }

    Ok(())
}

/// Load precompiles for Optimism chain.
#[inline]
pub fn load_precompiles<SPEC: Spec, EXT, DB: Database>() -> ContextPrecompiles<DB> {
    if SPEC::enabled(SpecId::ISTHMUS) {
        ContextPrecompiles::from_static_precompiles(optimism::precompile::isthmus())
    } else if SPEC::enabled(SpecId::GRANITE) {
        ContextPrecompiles::from_static_precompiles(optimism::precompile::granite())
    } else if SPEC::enabled(SpecId::FJORD) {
        ContextPrecompiles::from_static_precompiles(optimism::precompile::fjord())
    } else {
        ContextPrecompiles::new(PrecompileSpecId::from_spec_id(SPEC::SPEC_ID))
    }
}

/// Deduct max balance from caller
#[inline]
pub fn deduct_caller<SPEC: Spec, EXT, DB: Database>(
    context: &mut Context<EXT, DB>,
) -> Result<(), EVMError<DB::Error>> {
    // load caller's account.
    let mut caller_account = context
        .evm
        .inner
        .journaled_state
        .load_account(context.evm.inner.env.tx.caller, &mut context.evm.inner.db)?;

    // If the transaction is a deposit with a `mint` value, add the mint value
    // in wei to the caller's balance. This should be persisted to the database
    // prior to the rest of execution.
    if let Some(mint) = context.evm.inner.env.tx.optimism.mint {
        caller_account.info.balance += U256::from(mint);
    }

    // We deduct caller max balance after minting and before deducing the
    // l1 cost, max values is already checked in pre_validate but l1 cost wasn't.
    deduct_caller_inner::<SPEC>(caller_account.data, &context.evm.inner.env);

    // If the transaction is not a deposit transaction, subtract the L1 data fee from the
    // caller's balance directly after minting the requested amount of ETH.
    // Additionally deduct the operator fee from the caller's account.
    if context.evm.inner.env.tx.optimism.source_hash.is_none() {
        // get envelope
        let Some(enveloped_tx) = &context.evm.inner.env.tx.optimism.enveloped_tx else {
            return Err(EVMError::Custom(
                "[OPTIMISM] Failed to load enveloped transaction.".to_string(),
            ));
        };

        let l1_block = context
            .evm
            .inner
            .l1_block_info
            .as_mut()
            .expect("L1BlockInfo should be loaded");

        let tx_l1_cost = l1_block.calculate_tx_l1_cost(enveloped_tx, SPEC::SPEC_ID);
        caller_account.info.balance = caller_account.info.balance.saturating_sub(tx_l1_cost);

        // Deduct the operator fee from the caller's account.
        let gas_limit = U256::from(context.evm.inner.env.tx.gas_limit);

        let operator_fee_charge =
            l1_block.operator_fee_charge(enveloped_tx, gas_limit, SPEC::SPEC_ID);

        caller_account.info.balance = caller_account
            .info
            .balance
            .saturating_sub(operator_fee_charge);
    }
    Ok(())
}

/// Reward beneficiary with gas fee.
#[inline]
pub fn reward_beneficiary<SPEC: Spec, EXT, DB: Database>(
    context: &mut Context<EXT, DB>,
    gas: &Gas,
) -> Result<(), EVMError<DB::Error>> {
    let is_deposit = context.evm.inner.env.tx.optimism.source_hash.is_some();

    // transfer fee to coinbase/beneficiary.
    if !is_deposit {
        mainnet::reward_beneficiary::<SPEC, EXT, DB>(context, gas)?;
    }

    if !is_deposit {
        // If the transaction is not a deposit transaction, fees are paid out
        // to both the Base Fee Vault as well as the L1 Fee Vault.
        let Some(l1_block_info) = &mut context.evm.inner.l1_block_info else {
            return Err(EVMError::Custom(
                "[OPTIMISM] Failed to load L1 block information.".to_string(),
            ));
        };

        let Some(enveloped_tx) = &context.evm.inner.env.tx.optimism.enveloped_tx else {
            return Err(EVMError::Custom(
                "[OPTIMISM] Failed to load enveloped transaction.".to_string(),
            ));
        };

        let l1_cost = l1_block_info.calculate_tx_l1_cost(enveloped_tx, SPEC::SPEC_ID);
        let operator_fee_cost = l1_block_info.operator_fee_charge(
            enveloped_tx,
            U256::from(gas.spent() - gas.refunded() as u64),
            SPEC::SPEC_ID,
        );

        // Send the L1 cost of the transaction to the L1 Fee Vault.
        let mut l1_fee_vault_account = context
            .evm
            .inner
            .journaled_state
            .load_account(optimism::L1_FEE_RECIPIENT, &mut context.evm.inner.db)?;
        l1_fee_vault_account.mark_touch();
        l1_fee_vault_account.info.balance += l1_cost;

        // Send the base fee of the transaction to the Base Fee Vault.
        let mut base_fee_vault_account = context
            .evm
            .inner
            .journaled_state
            .load_account(optimism::BASE_FEE_RECIPIENT, &mut context.evm.inner.db)?;
        base_fee_vault_account.mark_touch();
        base_fee_vault_account.info.balance += context
            .evm
            .inner
            .env
            .block
            .basefee
            .mul(U256::from(gas.spent() - gas.refunded() as u64));

        // Send the operator fee of the transaction to the coinbase.
        let mut operator_fee_vault_account = context
            .evm
            .inner
            .journaled_state
            .load_account(OPERATOR_FEE_RECIPIENT, &mut context.evm.inner.db)?;

        operator_fee_vault_account.mark_touch();
        operator_fee_vault_account.data.info.balance += operator_fee_cost;
    }
    Ok(())
}

/// Main return handle, returns the output of the transaction.
#[inline]
pub fn output<SPEC: Spec, EXT, DB: Database>(
    context: &mut Context<EXT, DB>,
    frame_result: FrameResult,
) -> Result<ResultAndState, EVMError<DB::Error>> {
    let result = mainnet::output::<EXT, DB>(context, frame_result)?;

    if result.result.is_halt() {
        // Post-regolith, if the transaction is a deposit transaction and it halts,
        // we bubble up to the global return handler. The mint value will be persisted
        // and the caller nonce will be incremented there.
        let is_deposit = context.evm.inner.env.tx.optimism.source_hash.is_some();
        if is_deposit && SPEC::enabled(REGOLITH) {
            return Err(EVMError::Transaction(InvalidTransaction::OptimismError(
                OptimismInvalidTransaction::HaltedDepositPostRegolith,
            )));
        }
    }
    Ok(result)
}
/// Optimism end handle changes output if the transaction is a deposit transaction.
/// Deposit transaction can't be reverted and is always successful.
#[inline]
pub fn end<SPEC: Spec, EXT, DB: Database>(
    context: &mut Context<EXT, DB>,
    evm_output: Result<ResultAndState, EVMError<DB::Error>>,
) -> Result<ResultAndState, EVMError<DB::Error>> {
    evm_output.or_else(|err| {
        if matches!(err, EVMError::Transaction(_))
            && context.evm.inner.env().tx.optimism.source_hash.is_some()
        {
            // If the transaction is a deposit transaction and it failed
            // for any reason, the caller nonce must be bumped, and the
            // gas reported must be altered depending on the Hardfork. This is
            // also returned as a special Halt variant so that consumers can more
            // easily distinguish between a failed deposit and a failed
            // normal transaction.
            let caller = context.evm.inner.env().tx.caller;

            // Increment sender nonce and account balance for the mint amount. Deposits
            // always persist the mint amount, even if the transaction fails.
            let account = {
                let mut acc = Account::from(
                    context
                        .evm
                        .db
                        .basic(caller)
                        .unwrap_or_default()
                        .unwrap_or_default(),
                );
                acc.info.nonce = acc.info.nonce.saturating_add(1);
                acc.info.balance = acc.info.balance.saturating_add(U256::from(
                    context.evm.inner.env().tx.optimism.mint.unwrap_or(0),
                ));
                acc.mark_touch();
                acc
            };
            let state = HashMap::from_iter([(caller, account)]);

            // The gas used of a failed deposit post-regolith is the gas
            // limit of the transaction. pre-regolith, it is the gas limit
            // of the transaction for non system transactions and 0 for system
            // transactions.
            let is_system_tx = context
                .evm
                .env()
                .tx
                .optimism
                .is_system_transaction
                .unwrap_or(false);
            let gas_used = if SPEC::enabled(REGOLITH) || !is_system_tx {
                context.evm.inner.env().tx.gas_limit
            } else {
                0
            };

            Ok(ResultAndState {
                result: ExecutionResult::Halt {
                    reason: HaltReason::FailedDeposit,
                    gas_used,
                },
                state,
            })
        } else {
            Err(err)
        }
    })
}

/// Clears cache OP l1 value.
#[inline]
pub fn clear<EXT, DB: Database>(context: &mut Context<EXT, DB>) {
    // clear error and journaled state.
    mainnet::clear(context);
    context.evm.inner.l1_block_info = None;
}

#[cfg(test)]
mod tests {
    use revm_interpreter::{CallOutcome, InterpreterResult};

    use super::*;
    use crate::{
        db::{EmptyDB, InMemoryDB},
        primitives::{
            bytes, state::AccountInfo, Address, BedrockSpec, Bytes, Env, IsthmusSpec, LatestSpec,
            RegolithSpec, B256,
        },
        L1BlockInfo,
    };

    /// Creates frame result.
    fn call_last_frame_return<SPEC: Spec>(
        env: Env,
        instruction_result: InstructionResult,
        gas: Gas,
    ) -> Gas {
        let mut ctx = Context::new_empty();
        ctx.evm.inner.env = Box::new(env);
        let mut first_frame = FrameResult::Call(CallOutcome::new(
            InterpreterResult {
                result: instruction_result,
                output: Bytes::new(),
                gas,
            },
            0..0,
        ));
        last_frame_return::<SPEC, _, _>(&mut ctx, &mut first_frame).unwrap();
        refund::<SPEC, _, _>(&mut ctx, first_frame.gas_mut(), 0);
        *first_frame.gas()
    }

    #[test]
    fn test_revert_gas() {
        let mut env = Env::default();
        env.tx.gas_limit = 100;
        env.tx.optimism.source_hash = None;

        let gas =
            call_last_frame_return::<BedrockSpec>(env, InstructionResult::Revert, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas() {
        let mut env = Env::default();
        env.tx.gas_limit = 100;
        env.tx.optimism.source_hash = Some(B256::ZERO);

        let gas =
            call_last_frame_return::<RegolithSpec>(env, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_with_refund() {
        let mut env = Env::default();
        env.tx.gas_limit = 100;
        env.tx.optimism.source_hash = Some(B256::ZERO);

        let mut ret_gas = Gas::new(90);
        ret_gas.record_refund(20);

        let gas =
            call_last_frame_return::<RegolithSpec>(env.clone(), InstructionResult::Stop, ret_gas);
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 2); // min(20, 10/5)

        let gas = call_last_frame_return::<RegolithSpec>(env, InstructionResult::Revert, ret_gas);
        assert_eq!(gas.remaining(), 90);
        assert_eq!(gas.spent(), 10);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_consume_gas_sys_deposit_tx() {
        let mut env = Env::default();
        env.tx.gas_limit = 100;
        env.tx.optimism.source_hash = Some(B256::ZERO);

        let gas = call_last_frame_return::<BedrockSpec>(env, InstructionResult::Stop, Gas::new(90));
        assert_eq!(gas.remaining(), 0);
        assert_eq!(gas.spent(), 100);
        assert_eq!(gas.refunded(), 0);
    }

    #[test]
    fn test_commit_mint_value() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1000),
                ..Default::default()
            },
        );
        let mut context: Context<(), InMemoryDB> = Context::new_with_db(db);
        context.evm.inner.l1_block_info = Some(L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_fee_overhead: Some(U256::from(1_000)),
            l1_base_fee_scalar: U256::from(1_000),
            ..Default::default()
        });
        // Enveloped needs to be some but it will deduce zero fee.
        context.evm.inner.env.tx.optimism.enveloped_tx = Some(bytes!(""));
        // added mint value is 10.
        context.evm.inner.env.tx.optimism.mint = Some(10);

        deduct_caller::<RegolithSpec, (), _>(&mut context).unwrap();

        // Check the account balance is updated.
        let account = context
            .evm
            .inner
            .journaled_state
            .load_account(caller, &mut context.evm.inner.db)
            .unwrap();
        assert_eq!(account.info.balance, U256::from(1010));
    }

    #[test]
    fn test_remove_l1_cost_non_deposit() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1000),
                ..Default::default()
            },
        );
        let mut context: Context<(), InMemoryDB> = Context::new_with_db(db);
        context.evm.inner.l1_block_info = Some(L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_fee_overhead: Some(U256::from(1_000)),
            l1_base_fee_scalar: U256::from(1_000),
            ..Default::default()
        });
        // l1block cost is 1048 fee.
        context.evm.inner.env.tx.optimism.enveloped_tx = Some(bytes!("FACADE"));
        // added mint value is 10.
        context.evm.inner.env.tx.optimism.mint = Some(10);
        // Putting source_hash to some makes it a deposit transaction.
        // so enveloped_tx gas cost is ignored.
        context.evm.inner.env.tx.optimism.source_hash = Some(B256::ZERO);

        deduct_caller::<RegolithSpec, (), _>(&mut context).unwrap();

        // Check the account balance is updated.
        let account = context
            .evm
            .inner
            .journaled_state
            .load_account(caller, &mut context.evm.inner.db)
            .unwrap();
        assert_eq!(account.info.balance, U256::from(1010));
    }

    #[test]
    fn test_remove_l1_cost() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1049),
                ..Default::default()
            },
        );
        let mut context: Context<(), InMemoryDB> = Context::new_with_db(db);
        context.evm.inner.l1_block_info = Some(L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_fee_overhead: Some(U256::from(1_000)),
            l1_base_fee_scalar: U256::from(1_000),
            ..Default::default()
        });
        // l1block cost is 1048 fee.
        context.evm.inner.env.tx.optimism.enveloped_tx = Some(bytes!("FACADE"));
        deduct_caller::<RegolithSpec, (), _>(&mut context).unwrap();

        // Check the account balance is updated.
        let account = context
            .evm
            .inner
            .journaled_state
            .load_account(caller, &mut context.evm.inner.db)
            .unwrap();
        assert_eq!(account.info.balance, U256::from(1));
    }

    #[test]
    fn test_remove_operator_cost() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(151),
                ..Default::default()
            },
        );
        let mut context: Context<(), InMemoryDB> = Context::new_with_db(db);
        context.evm.l1_block_info = Some(L1BlockInfo {
            operator_fee_scalar: Some(U256::from(10_000_000)),
            operator_fee_constant: Some(U256::from(50)),
            ..Default::default()
        });
        context.evm.inner.env.tx.gas_limit = 10;

        // operator fee cost is operator_fee_scalar * gas_limit / 1e6 + operator_fee_constant
        // 10_000_000 * 10 / 1_000_000 + 50 = 150
        context.evm.inner.env.tx.optimism.enveloped_tx = Some(bytes!("FACADE"));
        deduct_caller::<IsthmusSpec, (), _>(&mut context).unwrap();

        // Check the account balance is updated.
        let account = context
            .evm
            .inner
            .journaled_state
            .load_account(caller, &mut context.evm.inner.db)
            .unwrap();
        assert_eq!(account.info.balance, U256::from(1));
    }

    #[test]
    fn test_remove_l1_cost_lack_of_funds() {
        let caller = Address::ZERO;
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(48),
                ..Default::default()
            },
        );
        let mut context: Context<(), InMemoryDB> = Context::new_with_db(db);
        context.evm.inner.l1_block_info = Some(L1BlockInfo {
            l1_base_fee: U256::from(1_000),
            l1_fee_overhead: Some(U256::from(1_000)),
            l1_base_fee_scalar: U256::from(1_000),
            ..Default::default()
        });
        // l1block cost is 1048 fee.
        context.evm.inner.env.tx.optimism.enveloped_tx = Some(bytes!("FACADE"));

        assert_eq!(
            validate_tx_against_state::<RegolithSpec, (), _>(&mut context),
            Err(EVMError::Transaction(
                InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(U256::from(1048)),
                    balance: Box::new(U256::from(48)),
                },
            ))
        );
    }

    #[test]
    fn test_validate_sys_tx() {
        // mark the tx as a system transaction.
        let mut env = Env::default();
        env.tx.optimism.is_system_transaction = Some(true);
        assert_eq!(
            validate_env::<RegolithSpec, EmptyDB>(&env),
            Err(EVMError::Transaction(InvalidTransaction::OptimismError(
                OptimismInvalidTransaction::DepositSystemTxPostRegolith
            )))
        );

        // Pre-regolith system transactions should be allowed.
        assert!(validate_env::<BedrockSpec, EmptyDB>(&env).is_ok());
    }

    #[test]
    fn test_validate_deposit_tx() {
        // Set source hash.
        let mut env = Env::default();
        env.tx.optimism.source_hash = Some(B256::ZERO);
        assert!(validate_env::<RegolithSpec, EmptyDB>(&env).is_ok());
    }

    #[test]
    fn test_validate_tx_against_state_deposit_tx() {
        // Set source hash.
        let mut env = Env::default();
        env.tx.optimism.source_hash = Some(B256::ZERO);

        // Nonce and balance checks should be skipped for deposit transactions.
        assert!(validate_env::<LatestSpec, EmptyDB>(&env).is_ok());
    }
}
