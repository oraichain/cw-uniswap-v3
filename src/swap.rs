use crate::error::UniswapV3MathError;
use crate::liquidity_math;
use crate::swap_math;
use crate::tick::Tick;
use crate::tick_bitmap;
use crate::tick_math;
use alloy::primitives::{I256, U256};
use std::collections::HashMap;

// the current state of the pool
pub struct Slot0 {
    // the current price
    pub sqrt_price: U256,
    pub liquidity: u128,
    // the current tick
    pub tick: i32,
}

#[derive(Debug)]
pub struct SwapResult {
    pub amount0_delta: I256,
    pub amount1_delta: I256,
    pub sqrt_price_after: U256,
    pub liquidity_after: u128,
    pub tick_after: i32,
}

// the top level state of the swap, the results of which are recorded in storage at the end
struct SwapState {
    amount_specified_remaining: I256,
    amount_calculated: I256,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
}

#[derive(Default)]
struct StepComputations {
    sqrt_price_start_x96: U256,
    tick_next: i32,
    initialized: bool,
    sqrt_price_next_x96: U256,
    amount_in: U256,
    amount_out: U256,
    fee_amount: U256,
}

pub fn swap(
    ticks: &HashMap<i32, Tick>,
    tick_bitmap: &HashMap<i16, U256>,
    tick_spacing: i32,
    zero_for_one: bool,
    amount_specified: I256,
    sqrt_price_limit: U256,
    slot0: &Slot0,
    fee: u32,
) -> Result<SwapResult, UniswapV3MathError> {
    if sqrt_price_limit <= tick_math::MIN_SQRT_RATIO {
        return Err(UniswapV3MathError::SplM);
    }
    if sqrt_price_limit >= tick_math::MAX_SQRT_RATIO {
        return Err(UniswapV3MathError::SpuM);
    }
    if zero_for_one {
        if sqrt_price_limit >= slot0.sqrt_price {
            return Err(UniswapV3MathError::SplC);
        }
    } else {
        if sqrt_price_limit <= slot0.sqrt_price {
            return Err(UniswapV3MathError::SpuC);
        }
    }
    let exact_input = amount_specified.is_positive();
    let mut state = SwapState {
        amount_specified_remaining: amount_specified,
        amount_calculated: I256::ZERO,
        sqrt_price_x96: slot0.sqrt_price,
        tick: slot0.tick,
        liquidity: slot0.liquidity,
    };
    while !state.amount_specified_remaining.is_zero() && state.sqrt_price_x96 != sqrt_price_limit {
        let mut step = StepComputations::default();
        step.sqrt_price_start_x96 = state.sqrt_price_x96;
        (step.tick_next, step.initialized) = tick_bitmap::next_initialized_tick_within_one_word(
            tick_bitmap,
            slot0.tick,
            tick_spacing,
            zero_for_one,
        )?;
        if step.tick_next < tick_math::MIN_TICK {
            step.tick_next = tick_math::MIN_TICK;
        } else if step.tick_next > tick_math::MAX_TICK {
            step.tick_next = tick_math::MAX_TICK;
        }
        step.sqrt_price_next_x96 = tick_math::get_sqrt_ratio_at_tick(step.tick_next)?;
        let hit_to_limit = if zero_for_one {
            // sell
            step.sqrt_price_next_x96 < sqrt_price_limit // The price of the next tick is lower than the limit
        } else {
            // buy
            step.sqrt_price_next_x96 > sqrt_price_limit // The price of the next tick is higher than the limit
        };
        let target_price = if hit_to_limit {
            sqrt_price_limit
        } else {
            step.sqrt_price_next_x96
        };
        // compute values to swap to the target tick, price limit, or point where input/output amount is exhausted
        (
            state.sqrt_price_x96,
            step.amount_in,
            step.amount_out,
            step.fee_amount,
        ) = swap_math::compute_swap_step(
            state.sqrt_price_x96,
            target_price,
            state.liquidity,
            state.amount_specified_remaining,
            fee,
        )?;
        if exact_input {
            state.amount_specified_remaining =
                state.amount_specified_remaining - I256::from_raw(step.amount_in + step.fee_amount);
            state.amount_calculated = state.amount_calculated - I256::from_raw(step.amount_out);
        } else {
            state.amount_specified_remaining =
                state.amount_specified_remaining + I256::from_raw(step.amount_out);
            state.amount_calculated =
                state.amount_calculated + I256::from_raw(step.amount_in + step.fee_amount);
        }
        // Do not calculate protocol fee
        if state.sqrt_price_x96 == step.sqrt_price_next_x96 {
            if step.initialized {
                // The initialized tick must exist in ticks
                let mut l_net = ticks.get(&step.tick_next).unwrap().liquidity_net;
                if zero_for_one {
                    l_net = -1 * l_net;
                }
                state.liquidity = liquidity_math::add_delta(state.liquidity, l_net)?;
            }
            if zero_for_one {
                state.tick = step.tick_next - 1
            } else {
                state.tick = step.tick_next
            }
        } else if state.sqrt_price_x96 != step.sqrt_price_start_x96 {
            state.tick = tick_math::get_tick_at_sqrt_ratio(state.sqrt_price_x96)?;
        }
    }
    let amount0_delta;
    let amount1_delta;
    if zero_for_one == exact_input {
        amount0_delta = amount_specified - state.amount_specified_remaining;
        amount1_delta = state.amount_calculated;
    } else {
        amount0_delta = state.amount_calculated;
        amount1_delta = amount_specified - state.amount_specified_remaining;
    }
    return Ok(SwapResult {
        amount0_delta,
        amount1_delta,
        sqrt_price_after: state.sqrt_price_x96,
        liquidity_after: state.liquidity,
        tick_after: state.tick,
    });
}

#[cfg(test)]
mod test {
    use super::{swap, Tick};
    use crate::{
        swap::Slot0,
        tick_bitmap::{flip_tick, next_initialized_tick_within_one_word},
        tick_math,
    };
    use alloy::primitives::{I256, U256};
    use std::{collections::HashMap, vec};

    pub fn init_test_ticks() -> eyre::Result<HashMap<i16, U256>> {
        let test_ticks = vec![-200, -55, -4, 70, 78, 84, 139, 240, 535];
        let mut tick_bitmap: HashMap<i16, U256> = HashMap::new();
        for tick in test_ticks {
            flip_tick(&mut tick_bitmap, tick, 1)?;
        }
        Ok(tick_bitmap)
    }

    #[test]
    pub fn test_swap() -> eyre::Result<()> {
        let tick_bitmap = init_test_ticks()?;
        //returns tick to right if at initialized tick
        let (next, initialized) =
            next_initialized_tick_within_one_word(&tick_bitmap, 78, 1, false)?;
        assert_eq!(next, 84);
        assert_eq!(initialized, true);

        let mut ticks: HashMap<i32, Tick> = HashMap::new();
        ticks.insert(
            1,
            Tick {
                liquidity_gross: 0,
                liquidity_net: 0,
                fee_growth_outside_0_x_128: U256::from(2),
                fee_growth_outside_1_x_128: U256::from(3),
                tick_cumulative_outside: U256::ZERO,
                seconds_per_liquidity_outside_x_128: U256::ZERO,
                seconds_outside: 9,
                initialized,
            },
        );

        let sqrt_price_limit = tick_math::MIN_SQRT_RATIO.wrapping_add(U256::from(1));

        let slot0 = &Slot0 {
            sqrt_price: sqrt_price_limit.wrapping_add(U256::from(1)),
            liquidity: 2_000_000u128,
            // the current tick
            tick: 1,
        };

        let swap_result = swap(
            &ticks,
            &tick_bitmap,
            1,
            true,
            I256::from_raw(U256::from(1_000_000)),
            sqrt_price_limit,
            &slot0,
            0,
        )?;

        println!("{:?}", swap_result);

        Ok(())
    }
}
