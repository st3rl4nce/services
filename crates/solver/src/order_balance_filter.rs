use {
    anyhow::{Context, Result},
    model::order::Order,
    primitive_types::{H160, U256},
    shared::account_balances::{BalanceFetching, Query},
    std::collections::HashMap,
};

/// An order processed by `balance_orders`.
///
/// To ensure that all orders passed to solvers are settleable we need to
/// make a choice for which orders to include when the user only has enough
/// sell token balance for some of them.
#[derive(Debug, Clone)]
pub struct BalancedOrder {
    pub order: Order,
    /// The amount of sell token balance that is usable by this order.
    ///
    /// The field might be larger than the order's sell_amount + fee_amount .
    ///
    /// The field might be smaller than the order's sell_amount + fee_amount for
    /// partially fillable orders. But it is always greater than 0 because no
    /// balance being available at all would make an order unsettleable.
    pub available_sell_token_balance: U256,
}

impl BalancedOrder {
    pub fn full(order: Order) -> Self {
        Self {
            order,
            available_sell_token_balance: U256::MAX,
        }
    }
}

/// Fetch the balances for `balance_orders`.
///
/// If a balance can't be fetched, it isn't part of the result.
pub async fn fetch_balances(
    fetcher: &dyn BalanceFetching,
    orders: &[Order],
) -> HashMap<Query, U256> {
    let queries: Vec<Query> = orders.iter().map(Query::from_order).collect();
    let balances = fetcher.get_balances(&queries).await;
    queries
        .into_iter()
        .zip(balances)
        .filter_map(|(query, balance)| balance.ok().map(|balance| (query, balance)))
        .collect()
}

#[cfg(test)]
pub fn max_balance(orders: &[Order]) -> HashMap<Query, U256> {
    orders
        .iter()
        .map(Query::from_order)
        .map(|q| (q, U256::MAX))
        .collect()
}

/// See the `BalancedOrder` documentation.
///
/// After the call, `balances` contains the remaining unassigned balances.
pub fn balance_orders(
    mut orders: Vec<Order>,
    balances: &mut HashMap<Query, U256>,
    ethflow_contract: Option<H160>,
) -> Vec<BalancedOrder> {
    orders.sort_by_key(|order| std::cmp::Reverse(order.metadata.creation_date));

    let mut result: Vec<BalancedOrder> = Vec::new();
    for order in orders {
        let key = Query::from_order(&order);
        let remaining_balance = match balances.get_mut(&key) {
            Some(balance) => balance,
            None => continue,
        };

        // For ethflow orders, there is no need to check the balance. The contract
        // ensures that there will always be sufficient balance, after the wrapAll
        // pre_interaction has been called.
        if Some(order.metadata.owner) == ethflow_contract {
            result.push(BalancedOrder::full(order));
            continue;
        }

        let needed_balance = match max_transfer_out_amount(&order) {
            // Should only ever happen if a partially fillable order has been filled completely
            Ok(balance) if balance.is_zero() => continue,
            Ok(balance) => balance,
            Err(err) => {
                // This should only happen if we read bogus order data from
                // the database (either we allowed a bogus order to be
                // created or we updated a good order incorrectly), so raise
                // the alarm!
                tracing::error!(
                    ?err,
                    ?order,
                    "error computing order max transfer out amount"
                );
                continue;
            }
        };

        if order.data.partially_fillable {
            let balance_for_order = std::cmp::min(needed_balance, *remaining_balance);
            if balance_for_order == 0.into() {
                continue;
            }
            result.push(BalancedOrder {
                order,
                available_sell_token_balance: balance_for_order,
            });
            *remaining_balance -= balance_for_order;
        } else {
            if needed_balance > *remaining_balance {
                continue;
            }
            result.push(BalancedOrder::full(order));
            *remaining_balance -= needed_balance;
        }
    }

    result
}

/// Computes the maximum amount that can be transferred out for a given order.
///
/// While this is trivial for fill or kill orders (`sell_amount + fee_amount`),
/// partially fillable orders need to account for the already filled amount (so
/// a half-filled order would be `(sell_amount + fee_amount) / 2`).
///
/// Returns `Err` on overflow.
fn max_transfer_out_amount(order: &Order) -> Result<U256> {
    let remaining = shared::remaining_amounts::Remaining::from_order(order)?;
    let sell = remaining.remaining(order.data.sell_amount)?;
    let fee = remaining.remaining(order.data.fee_amount)?;
    sell.checked_add(fee).context("add")
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        chrono::{TimeZone, Utc},
        maplit::hashmap,
        model::order::{OrderData, OrderMetadata},
    };

    #[tokio::test]
    async fn filters_insufficient_balances() {
        let mut orders = vec![
            Order {
                data: OrderData {
                    sell_amount: 3.into(),
                    fee_amount: 3.into(),
                    ..Default::default()
                },
                metadata: OrderMetadata {
                    creation_date: Utc.timestamp_opt(2, 0).unwrap(),
                    ..Default::default()
                },
                ..Default::default()
            },
            Order {
                data: OrderData {
                    sell_amount: 2.into(),
                    fee_amount: 2.into(),
                    ..Default::default()
                },
                metadata: OrderMetadata {
                    creation_date: Utc.timestamp_opt(1, 0).unwrap(),
                    ..Default::default()
                },
                ..Default::default()
            },
            Order {
                data: OrderData {
                    sell_amount: 10.into(),
                    buy_amount: 10.into(),
                    fee_amount: 10.into(),
                    partially_fillable: true,
                    ..Default::default()
                },
                metadata: OrderMetadata {
                    creation_date: Utc.timestamp_opt(0, 0).unwrap(),
                    ..Default::default()
                },
                ..Default::default()
            },
        ];

        let mut balances = hashmap! {Query::from_order(&orders[0]) => U256::from(9)};
        let orders_ = balance_orders(orders.clone(), &mut balances, None);
        assert_eq!(orders_.len(), 2);
        // Second order has lower timestamp so it isn't picked.
        assert_eq!(orders_[0].order.data, orders[0].data);
        // Third order is partially fillable so is picked with remaining balance.
        assert_eq!(orders_[1].order.data, orders[2].data);
        assert_eq!(orders_[1].available_sell_token_balance, 3.into());
        assert_eq!(
            balances.get(&Query::from_order(&orders[0])),
            Some(&0.into())
        );

        orders[1].metadata.creation_date = Utc.timestamp_opt(3, 0).unwrap();
        let mut balances = hashmap! {Query::from_order(&orders[0]) => U256::from(9)};
        let orders_ = balance_orders(orders.clone(), &mut balances, None);
        assert_eq!(orders_.len(), 2);
        assert_eq!(orders_[0].order.data, orders[1].data);
        // Remaining balance is different because previous order has changed.
        assert_eq!(orders_[1].order.data, orders[2].data);
        assert_eq!(orders_[1].available_sell_token_balance, 5.into());
        assert_eq!(
            balances.get(&Query::from_order(&orders[0])),
            Some(&0.into())
        );
    }

    #[tokio::test]
    async fn records_remaining_balance() {
        let orders = vec![Order {
            data: OrderData {
                sell_amount: 3.into(),
                fee_amount: 3.into(),
                ..Default::default()
            },
            metadata: OrderMetadata {
                creation_date: Utc.timestamp_opt(2, 0).unwrap(),
                ..Default::default()
            },
            ..Default::default()
        }];

        let mut balances = hashmap! {Query::from_order(&orders[0]) => U256::from(9)};
        let orders_ = balance_orders(orders.clone(), &mut balances, None);
        // 6 balance assigned to order, 3 remaining
        assert_eq!(orders_.len(), 1);
        assert_eq!(
            balances.get(&Query::from_order(&orders[0])),
            Some(&3.into())
        );
    }

    #[tokio::test]
    async fn do_not_filters_insufficient_balances_for_ethflow_orders() {
        let ethflow_address = H160([3u8; 20]);
        let orders = vec![Order {
            data: OrderData {
                sell_amount: 3.into(),
                fee_amount: 3.into(),
                ..Default::default()
            },
            metadata: OrderMetadata {
                creation_date: Utc.timestamp_opt(2, 0).unwrap(),
                owner: ethflow_address,
                ..Default::default()
            },
            ..Default::default()
        }];

        let mut balances = hashmap! {Query::from_order(&orders[0]) => U256::from(0)};
        let orders_ = balance_orders(orders.clone(), &mut balances, Some(ethflow_address));
        assert_eq!(orders_.len(), 1);
        assert_eq!(orders_[0].order, orders[0]);
    }

    #[test]
    fn filters_zero_amount_orders() {
        let orders = vec![
            // normal order with non zero amounts
            Order {
                data: OrderData {
                    buy_amount: 1u8.into(),
                    sell_amount: 1u8.into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            // partially fillable order with remaining liquidity
            Order {
                data: OrderData {
                    partially_fillable: true,
                    buy_amount: 1u8.into(),
                    sell_amount: 1u8.into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            // normal order with zero amounts
            Order::default(),
            // partially fillable order completely filled
            Order {
                metadata: OrderMetadata {
                    executed_buy_amount: 1u8.into(),
                    executed_sell_amount: 1u8.into(),
                    ..Default::default()
                },
                data: OrderData {
                    partially_fillable: true,
                    buy_amount: 1u8.into(),
                    sell_amount: 1u8.into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        ];

        let mut balances = hashmap! {Query::from_order(&orders[0]) => U256::MAX};
        let expected_result = vec![orders[0].clone(), orders[1].clone()];
        let mut filtered_orders = balance_orders(orders, &mut balances, None);
        // Deal with `solvable_orders()` sorting the orders.
        filtered_orders.sort_by_key(|order| order.order.metadata.creation_date);
        assert_eq!(expected_result.len(), filtered_orders.len());
        for (left, right) in expected_result.iter().zip(filtered_orders) {
            assert_eq!(left.data, right.order.data);
        }
    }
}
