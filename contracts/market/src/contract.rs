#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use crate::borrow::{
    borrow_stable, claim_rewards, compute_interest, compute_interest_raw, compute_reward,
    query_borrower_info, query_borrower_infos, repay_stable, repay_stable_from_liquidation,
};
use crate::deposit::{compute_exchange_rate_raw, deposit_stable, redeem_stable};
use crate::error::ContractError;
use crate::querier::{query_anc_emission_rate, query_borrow_rate, query_target_deposit_rate};
use crate::response::MsgInstantiateContractResponse;
use crate::state::{read_config, read_state, store_config, store_state, Config, State};

use cosmwasm_bignumber::{Decimal256, Uint256};
use cosmwasm_std::{
    attr, from_binary, to_binary, Addr, BankMsg, Binary, CanonicalAddr, Coin, CosmosMsg, Deps,
    DepsMut, Env, MessageInfo, Reply, Response, StdError, StdResult, SubMsg, Uint128, WasmMsg,
};
use cw20::{Cw20Coin, Cw20ReceiveMsg, MinterResponse};

use moneymarket::common::optional_addr_validate;
use moneymarket::interest_model::BorrowRateResponse;
use moneymarket::market::{
    ConfigResponse, Cw20HookMsg, EpochStateResponse, ExecuteMsg, InstantiateMsg, MigrateMsg,
    QueryMsg, StateResponse,
};
use moneymarket::querier::{deduct_tax, query_balance, query_supply};
use protobuf::Message;
use terraswap::token::InstantiateMsg as TokenInstantiateMsg;

pub const INITIAL_DEPOSIT_AMOUNT: u128 = 1000000;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    let initial_deposit = info
        .funds
        .iter()
        .find(|c| c.denom == msg.stable_denom)
        .map(|c| c.amount)
        .unwrap_or_else(Uint128::zero);

    if initial_deposit != Uint128::from(INITIAL_DEPOSIT_AMOUNT) {
        return Err(ContractError::InitialFundsNotDeposited(
            INITIAL_DEPOSIT_AMOUNT,
            msg.stable_denom,
        ));
    }

    store_config(
        deps.storage,
        &Config {
            contract_addr: deps.api.addr_canonicalize(env.contract.address.as_str())?,
            owner_addr: deps.api.addr_canonicalize(&msg.owner_addr)?,
            aterra_contract: CanonicalAddr::from(vec![]),
            overseer_contract: CanonicalAddr::from(vec![]),
            interest_model: CanonicalAddr::from(vec![]),
            distribution_model: CanonicalAddr::from(vec![]),
            collector_contract: CanonicalAddr::from(vec![]),
            distributor_contract: CanonicalAddr::from(vec![]),
            stable_denom: msg.stable_denom.clone(),
            max_borrow_factor: msg.max_borrow_factor,
        },
    )?;

    store_state(
        deps.storage,
        &State {
            total_liabilities: Decimal256::zero(),
            total_reserves: Decimal256::zero(),
            last_interest_updated_time: env.block.time.seconds(),
            last_reward_updated_time: env.block.time.seconds(),
            global_interest_index: Decimal256::one(),
            global_reward_index: Decimal256::zero(),
            anc_emission_rate: msg.anc_emission_rate,
            prev_aterra_supply: Uint256::zero(),
            prev_exchange_rate: Decimal256::one(),
        },
    )?;

    Ok(
        Response::new().add_submessages(vec![SubMsg::reply_on_success(
            CosmosMsg::Wasm(WasmMsg::Instantiate {
                admin: None,
                code_id: msg.aterra_code_id,
                funds: vec![],
                label: "".to_string(),
                msg: to_binary(&TokenInstantiateMsg {
                    name: format!("Anchor Terra {}", msg.stable_denom[1..].to_uppercase()),
                    symbol: format!(
                        "a{}T",
                        msg.stable_denom[1..(msg.stable_denom.len() - 1)].to_uppercase()
                    ),
                    decimals: 6u8,
                    initial_balances: vec![Cw20Coin {
                        address: env.contract.address.to_string(),
                        amount: Uint128::from(INITIAL_DEPOSIT_AMOUNT),
                    }],
                    mint: Some(MinterResponse {
                        minter: env.contract.address.to_string(),
                        cap: None,
                    }),
                })?,
            }),
            1,
        )]),
    )
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::RegisterContracts {
            overseer_contract,
            interest_model,
            distribution_model,
            collector_contract,
            distributor_contract,
        } => {
            let api = deps.api;
            register_contracts(
                deps,
                api.addr_validate(&overseer_contract)?,
                api.addr_validate(&interest_model)?,
                api.addr_validate(&distribution_model)?,
                api.addr_validate(&collector_contract)?,
                api.addr_validate(&distributor_contract)?,
            )
        }
        ExecuteMsg::UpdateConfig {
            owner_addr,
            interest_model,
            distribution_model,
            max_borrow_factor,
        } => {
            let api = deps.api;
            update_config(
                deps,
                env,
                info,
                optional_addr_validate(api, owner_addr)?,
                optional_addr_validate(api, interest_model)?,
                optional_addr_validate(api, distribution_model)?,
                max_borrow_factor,
            )
        }
        ExecuteMsg::ExecuteEpochOperations {
            deposit_rate,
            target_deposit_rate,
            threshold_deposit_rate,
            distributed_interest,
        } => execute_epoch_operations(
            deps,
            env,
            info,
            deposit_rate,
            target_deposit_rate,
            threshold_deposit_rate,
            distributed_interest,
        ),
        ExecuteMsg::DepositStable {} => deposit_stable(deps, env, info),
        ExecuteMsg::BorrowStable { borrow_amount, to } => {
            let api = deps.api;
            borrow_stable(
                deps,
                env,
                info,
                borrow_amount,
                optional_addr_validate(api, to)?,
            )
        }
        ExecuteMsg::RepayStable {} => repay_stable(deps, env, info),
        ExecuteMsg::RepayStableFromLiquidation {
            borrower,
            prev_balance,
        } => {
            let api = deps.api;
            repay_stable_from_liquidation(
                deps,
                env,
                info,
                api.addr_validate(&borrower)?,
                prev_balance,
            )
        }
        ExecuteMsg::ClaimRewards { to } => {
            let api = deps.api;
            claim_rewards(deps, env, info, optional_addr_validate(api, to)?)
        }
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, _env: Env, msg: Reply) -> Result<Response, ContractError> {
    match msg.id {
        1 => {
            // get new token's contract address
            let res: MsgInstantiateContractResponse = Message::parse_from_bytes(
                msg.result.unwrap().data.unwrap().as_slice(),
            )
            .map_err(|_| {
                ContractError::Std(StdError::parse_err(
                    "MsgInstantiateContractResponse",
                    "failed to parse data",
                ))
            })?;
            let token_addr = Addr::unchecked(res.get_contract_address());

            register_aterra(deps, token_addr)
        }
        _ => Err(ContractError::InvalidReplyId {}),
    }
}

pub fn receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let contract_addr = info.sender;
    match from_binary(&cw20_msg.msg) {
        Ok(Cw20HookMsg::RedeemStable {}) => {
            // only asset contract can execute this message
            let config: Config = read_config(deps.storage)?;
            if deps.api.addr_canonicalize(contract_addr.as_str())? != config.aterra_contract {
                return Err(ContractError::Unauthorized {});
            }

            let cw20_sender_addr = deps.api.addr_validate(&cw20_msg.sender)?;
            redeem_stable(deps, env, cw20_sender_addr, cw20_msg.amount)
        }
        _ => Err(ContractError::MissingRedeemStableHook {}),
    }
}

pub fn register_aterra(deps: DepsMut, token_addr: Addr) -> Result<Response, ContractError> {
    let mut config: Config = read_config(deps.storage)?;
    if config.aterra_contract != CanonicalAddr::from(vec![]) {
        return Err(ContractError::Unauthorized {});
    }

    config.aterra_contract = deps.api.addr_canonicalize(token_addr.as_str())?;
    store_config(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![attr("aterra", token_addr)]))
}

pub fn register_contracts(
    deps: DepsMut,
    overseer_contract: Addr,
    interest_model: Addr,
    distribution_model: Addr,
    collector_contract: Addr,
    distributor_contract: Addr,
) -> Result<Response, ContractError> {
    let mut config: Config = read_config(deps.storage)?;
    if config.overseer_contract != CanonicalAddr::from(vec![])
        || config.interest_model != CanonicalAddr::from(vec![])
        || config.distribution_model != CanonicalAddr::from(vec![])
        || config.collector_contract != CanonicalAddr::from(vec![])
        || config.distributor_contract != CanonicalAddr::from(vec![])
    {
        return Err(ContractError::Unauthorized {});
    }

    config.overseer_contract = deps.api.addr_canonicalize(overseer_contract.as_str())?;
    config.interest_model = deps.api.addr_canonicalize(interest_model.as_str())?;
    config.distribution_model = deps.api.addr_canonicalize(distribution_model.as_str())?;
    config.collector_contract = deps.api.addr_canonicalize(collector_contract.as_str())?;
    config.distributor_contract = deps.api.addr_canonicalize(distributor_contract.as_str())?;
    store_config(deps.storage, &config)?;

    Ok(Response::default())
}

pub fn update_config(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    owner_addr: Option<Addr>,
    interest_model: Option<Addr>,
    distribution_model: Option<Addr>,
    max_borrow_factor: Option<Decimal256>,
) -> Result<Response, ContractError> {
    let mut config: Config = read_config(deps.storage)?;

    // permission check
    if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner_addr {
        return Err(ContractError::Unauthorized {});
    }

    if let Some(owner_addr) = owner_addr {
        config.owner_addr = deps.api.addr_canonicalize(owner_addr.as_str())?;
    }

    if interest_model.is_some() {
        let mut state: State = read_state(deps.storage)?;
        compute_interest(
            deps.as_ref(),
            &config,
            &mut state,
            env.block.time.seconds(),
            None,
        )?;
        store_state(deps.storage, &state)?;

        if let Some(interest_model) = interest_model {
            config.interest_model = deps.api.addr_canonicalize(interest_model.as_str())?;
        }
    }

    if let Some(distribution_model) = distribution_model {
        config.distribution_model = deps.api.addr_canonicalize(distribution_model.as_str())?;
    }

    if let Some(max_borrow_factor) = max_borrow_factor {
        config.max_borrow_factor = max_borrow_factor;
    }

    store_config(deps.storage, &config)?;
    Ok(Response::new().add_attributes(vec![attr("action", "update_config")]))
}

pub fn execute_epoch_operations(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    deposit_rate: Decimal256,
    target_deposit_rate: Decimal256,
    threshold_deposit_rate: Decimal256,
    distributed_interest: Uint256,
) -> Result<Response, ContractError> {
    let config: Config = read_config(deps.storage)?;
    if config.overseer_contract != deps.api.addr_canonicalize(info.sender.as_str())? {
        return Err(ContractError::Unauthorized {});
    }

    let mut state: State = read_state(deps.storage)?;

    // Compute interest and reward before updating anc_emission_rate
    let aterra_supply = query_supply(
        deps.as_ref(),
        deps.api.addr_humanize(&config.aterra_contract)?,
    )?;
    let balance: Uint256 = query_balance(
        deps.as_ref(),
        deps.api.addr_humanize(&config.contract_addr)?,
        config.stable_denom.to_string(),
    )? - distributed_interest;

    let borrow_rate_res: BorrowRateResponse = query_borrow_rate(
        deps.as_ref(),
        deps.api.addr_humanize(&config.interest_model)?,
        balance,
        state.total_liabilities,
        state.total_reserves,
    )?;

    compute_interest_raw(
        &mut state,
        env.block.time.seconds(),
        balance,
        aterra_supply,
        borrow_rate_res.rate,
        target_deposit_rate,
    );

    // recompute prev_exchange_rate with distributed_interest
    state.prev_exchange_rate =
        compute_exchange_rate_raw(&state, aterra_supply, balance + distributed_interest);

    compute_reward(&mut state, env.block.time.seconds());

    // Compute total_reserves to fund collector contract
    // Update total_reserves and send it to collector contract
    // only when there is enough balance
    let total_reserves = state.total_reserves * Uint256::one();
    let messages: Vec<CosmosMsg> = if !total_reserves.is_zero() && balance > total_reserves {
        state.total_reserves = state.total_reserves - Decimal256::from_uint256(total_reserves);

        vec![CosmosMsg::Bank(BankMsg::Send {
            to_address: deps
                .api
                .addr_humanize(&config.collector_contract)?
                .to_string(),
            amount: vec![deduct_tax(
                deps.as_ref(),
                Coin {
                    denom: config.stable_denom,
                    amount: total_reserves.into(),
                },
            )?],
        })]
    } else {
        vec![]
    };

    // Query updated anc_emission_rate
    state.anc_emission_rate = query_anc_emission_rate(
        deps.as_ref(),
        deps.api.addr_humanize(&config.distribution_model)?,
        deposit_rate,
        target_deposit_rate,
        threshold_deposit_rate,
        state.anc_emission_rate,
    )?
    .emission_rate;

    store_state(deps.storage, &state)?;

    Ok(Response::new().add_messages(messages).add_attributes(vec![
        attr("action", "execute_epoch_operations"),
        attr("total_reserves", total_reserves),
        attr("anc_emission_rate", state.anc_emission_rate.to_string()),
    ]))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
        QueryMsg::State { block_time } => to_binary(&query_state(deps, env, block_time)?),
        QueryMsg::EpochState {
            block_time,
            distributed_interest,
        } => to_binary(&query_epoch_state(deps, block_time, distributed_interest)?),
        QueryMsg::BorrowerInfo {
            borrower,
            block_time,
        } => to_binary(&query_borrower_info(
            deps,
            env,
            deps.api.addr_validate(&borrower)?,
            block_time,
        )?),
        QueryMsg::BorrowerInfos { start_after, limit } => to_binary(&query_borrower_infos(
            deps,
            optional_addr_validate(deps.api, start_after)?,
            limit,
        )?),
    }
}

pub fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config: Config = read_config(deps.storage)?;
    Ok(ConfigResponse {
        owner_addr: deps.api.addr_humanize(&config.owner_addr)?.to_string(),
        aterra_contract: deps.api.addr_humanize(&config.aterra_contract)?.to_string(),
        interest_model: deps.api.addr_humanize(&config.interest_model)?.to_string(),
        distribution_model: deps
            .api
            .addr_humanize(&config.distribution_model)?
            .to_string(),
        overseer_contract: deps
            .api
            .addr_humanize(&config.overseer_contract)?
            .to_string(),
        collector_contract: deps
            .api
            .addr_humanize(&config.collector_contract)?
            .to_string(),
        distributor_contract: deps
            .api
            .addr_humanize(&config.distributor_contract)?
            .to_string(),
        stable_denom: config.stable_denom,
        max_borrow_factor: config.max_borrow_factor,
    })
}

pub fn query_state(deps: Deps, env: Env, block_time: Option<u64>) -> StdResult<StateResponse> {
    let mut state: State = read_state(deps.storage)?;

    let block_time = if let Some(block_time) = block_time {
        block_time
    } else {
        env.block.time.seconds()
    };

    if block_time < state.last_interest_updated_time {
        return Err(StdError::generic_err(
            "block_height must bigger than last_interest_updated",
        ));
    }

    if block_time < state.last_reward_updated_time {
        return Err(StdError::generic_err(
            "block_height must bigger than last_reward_updated",
        ));
    }

    let config: Config = read_config(deps.storage)?;

    // Compute interest rate with given block height
    compute_interest(deps, &config, &mut state, block_time, None)?;

    // Compute reward rate with given block height
    compute_reward(&mut state, block_time);

    Ok(StateResponse {
        total_liabilities: state.total_liabilities,
        total_reserves: state.total_reserves,
        last_interest_updated_time: state.last_interest_updated_time,
        last_reward_updated_time: state.last_reward_updated_time,
        global_interest_index: state.global_interest_index,
        global_reward_index: state.global_reward_index,
        anc_emission_rate: state.anc_emission_rate,
        prev_aterra_supply: state.prev_aterra_supply,
        prev_exchange_rate: state.prev_exchange_rate,
    })
}

pub fn query_epoch_state(
    deps: Deps,
    block_time: Option<u64>,
    distributed_interest: Option<Uint256>,
) -> StdResult<EpochStateResponse> {
    let config: Config = read_config(deps.storage)?;
    let mut state: State = read_state(deps.storage)?;

    let distributed_interest = distributed_interest.unwrap_or_else(Uint256::zero);
    let aterra_supply = query_supply(deps, deps.api.addr_humanize(&config.aterra_contract)?)?;
    let balance = query_balance(
        deps,
        deps.api.addr_humanize(&config.contract_addr)?,
        config.stable_denom.to_string(),
    )? - distributed_interest;

    if let Some(block_time) = block_time {
        if block_time < state.last_interest_updated_time {
            return Err(StdError::generic_err(
                "block_height must bigger than last_interest_updated",
            ));
        }

        let borrow_rate_res: BorrowRateResponse = query_borrow_rate(
            deps,
            deps.api.addr_humanize(&config.interest_model)?,
            balance,
            state.total_liabilities,
            state.total_reserves,
        )?;

        let target_deposit_rate: Decimal256 =
            query_target_deposit_rate(deps, deps.api.addr_humanize(&config.overseer_contract)?)?;

        // Compute interest rate to return latest epoch state
        compute_interest_raw(
            &mut state,
            block_time,
            balance,
            aterra_supply,
            borrow_rate_res.rate,
            target_deposit_rate,
        );
    }

    // compute_interest_raw store current exchange rate
    // as prev_exchange_rate, so just return prev_exchange_rate
    let exchange_rate =
        compute_exchange_rate_raw(&state, aterra_supply, balance + distributed_interest);

    Ok(EpochStateResponse {
        exchange_rate,
        aterra_supply,
    })
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, msg: MigrateMsg) -> StdResult<Response> {
    //read and store  the state and replace the prev anc emission rate
    // with the new value based on time
    // prev value was based on block
    let mut state = read_state(deps.storage)?;
    state.anc_emission_rate = msg.anc_emission_rate;

    store_state(deps.storage, &state)?;

    Ok(Response::default())
}

#[cfg(test)]
mod test {
    use super::*;
    use cosmwasm_std::testing::{mock_dependencies, mock_env, mock_info};
    use std::str::FromStr;

    #[test]
    fn proper_migrate() {
        let mut deps = mock_dependencies(&[]);

        let legacy_anc_emssion_rate = Decimal256::from_str("1").unwrap();
        let new_anc_emission_rate = Decimal256::from_str("1.1").unwrap();

        // init the contract
        let init_msg = InstantiateMsg {
            owner_addr: "owner".to_string(),
            stable_denom: "uusd".to_string(),
            aterra_code_id: 0,
            anc_emission_rate: Decimal256::from_str("1").unwrap(),
            max_borrow_factor: Default::default(),
        };

        let info = mock_info("sender", &[Coin::new(1000000, "uusd")]);
        let res = instantiate(deps.as_mut(), mock_env(), info, init_msg).unwrap();
        assert_eq!(1, res.messages.len());

        let state = read_state(&deps.storage).unwrap();
        assert_eq!(state.anc_emission_rate, legacy_anc_emssion_rate);

        // migrate
        let migrate_msg = MigrateMsg {
            anc_emission_rate: new_anc_emission_rate,
        };
        let res = migrate(deps.as_mut(), mock_env(), migrate_msg).unwrap();
        assert_eq!(res, Response::default());

        let state = read_state(&deps.storage).unwrap();
        assert_eq!(state.anc_emission_rate, new_anc_emission_rate)
    }
}
