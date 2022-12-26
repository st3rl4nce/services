use {
    crate::{
        blockchain,
        boundary,
        logic::{
            competition::{self, order},
            eth,
        },
        simulator,
        solver::{self, Solver},
        Ethereum,
        Simulator,
    },
    futures::future::try_join_all,
    itertools::Itertools,
    num::ToPrimitive,
    primitive_types::U256,
    rand::Rng,
    std::collections::HashMap,
};

pub mod interaction;
mod settlement;
pub mod trade;

pub use {
    interaction::{Interaction, Interactions},
    settlement::Settlement,
    trade::Trade,
};

/// A solution represents a set of orders which the solver has found an optimal
/// way to settle. A [`Solution`] is generated by a solver as a response to a
/// [`super::auction::Auction`].
#[derive(Debug)]
pub struct Solution {
    /// Trades settled by this solution.
    pub trades: Vec<Trade>,
    /// Token prices for this solution.
    pub prices: HashMap<eth::TokenAddress, competition::Price>,
    pub interactions: Vec<Interaction>,
    /// The solver which generated this solution.
    pub solver: Solver,
}

impl Solution {
    pub async fn approvals(
        &self,
        eth: &Ethereum,
    ) -> Result<impl Iterator<Item = eth::allowance::Approval>, blockchain::Error> {
        let settlement_contract = &eth.contracts().settlement();
        let allowances = try_join_all(self.allowances().map(|required| async move {
            eth.allowance(settlement_contract.address().into(), required.0.spender)
                .await
                .map(|existing| (required, existing))
        }))
        .await?;
        let approvals = allowances.into_iter().filter_map(|(required, existing)| {
            required
                .approval(&existing)
                // As a gas optimization, we always approve the max amount possible. This minimizes
                // the number of approvals necessary, and therefore minimizes the approval fees over time. This is a
                // potential security issue, but its effects are minimized and only exploitable if
                // solvers use insecure contracts.
                .map(eth::allowance::Approval::max)
        });
        Ok(approvals)
    }

    /// Return the trades which fulfill non-liquidity auction orders. These are
    /// the orders placed by end-users.
    fn user_trades(&self) -> impl Iterator<Item = &trade::Fulfillment> {
        self.trades.iter().filter_map(|trade| match trade {
            Trade::Fulfillment(fulfillment) => match fulfillment.order.kind {
                order::Kind::Market | order::Kind::Limit { .. } => Some(fulfillment),
                order::Kind::Liquidity => None,
            },
            Trade::Jit(_) => None,
        })
    }

    /// Return the allowances in a normalized form, where there is only one
    /// allowance per [`eth::allowance::Spender`], and they're ordered
    /// deterministically.
    fn allowances(&self) -> impl Iterator<Item = eth::allowance::Required> {
        let mut normalized = HashMap::new();
        let allowances = self
            .interactions
            .iter()
            .flat_map(|interaction| match interaction {
                Interaction::Custom(interaction) => interaction.allowances.clone().into_iter(),
                Interaction::Liquidity(interaction) => vec![eth::Allowance {
                    spender: eth::allowance::Spender {
                        // TODO This is a mistake, right? I think this should be the settlement
                        // contract address?
                        address: interaction.liquidity.address,
                        token: interaction.output.token,
                    },
                    amount: interaction.output.amount,
                }
                .into()]
                .into_iter(),
            });
        for allowance in allowances {
            let amount = normalized
                .entry(allowance.0.spender)
                .or_insert(U256::zero());
            *amount = amount.saturating_add(allowance.0.amount);
        }
        normalized
            .into_iter()
            .map(|(spender, amount)| eth::Allowance { spender, amount }.into())
            .sorted()
    }

    /// Simulate this solution on the blockchain.
    async fn simulate(
        &self,
        eth: &Ethereum,
        simulator: &Simulator,
        // TODO Remove the auction parameter in a follow-up
        auction: &competition::Auction,
    ) -> Result<eth::Gas, Error> {
        // Our settlement contract will fail if the receiver is a smart contract.
        // Because of this, if the receiver is a smart contract and we try to
        // estimate the access list, the access list estimation will also fail.
        //
        // This failure happens is because the Ethereum protocol sets a hard gas limit
        // on transferring ETH into a smart contract, which some contracts exceed unless
        // the access list is already specified.

        // The solution is to do access list estimation in two steps: first, simulate
        // moving 1 wei into every smart contract to get a partial access list.
        let partial_access_lists = try_join_all(
            self.user_trades()
                .map(|trade| async { self.partial_access_list(eth, simulator, trade).await }),
        )
        .await?;
        let partial_access_list = partial_access_lists
            .into_iter()
            .fold(eth::AccessList::default(), |acc, list| acc.merge(list));

        // Encode the settlement with the partial access list.
        let settlement = Settlement::encode(eth, auction, self)
            .await?
            .tx()
            .merge_access_list(partial_access_list);

        // Second, simulate the full access list, passing the partial access
        // list into the simulation. This way the settlement contract does not
        // fail, and hence the full access list estimation also does not fail.
        let settlement = simulator.access_list(settlement).await?;

        // Finally, get the gas for the settlement with the full access list.
        simulator.gas(&settlement).await.map_err(Into::into)
    }

    async fn partial_access_list(
        &self,
        eth: &Ethereum,
        simulator: &Simulator,
        trade: &trade::Fulfillment,
    ) -> Result<eth::AccessList, Error> {
        if !trade.order.buys_eth() || !trade.order.pays_to_contract(eth).await? {
            return Ok(Default::default());
        }
        let tx = eth::Tx {
            from: self.solver.account(),
            to: trade.order.receiver(),
            value: 1.into(),
            input: Vec::new(),
            access_list: Default::default(),
        };
        Ok(simulator.access_list(tx).await?.access_list)
    }
}

/// The solution score. This is often referred to as the "objective value".
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct Score(pub num::BigRational);

impl From<Score> for f64 {
    fn from(score: Score) -> Self {
        score.0.to_f64().expect("value can be represented as f64")
    }
}

impl From<num::BigRational> for Score {
    fn from(inner: num::BigRational) -> Self {
        Self(inner)
    }
}

/// Solve an auction and return the [`Score`] of the solution.
pub async fn solve(
    solver: Solver,
    eth: &Ethereum,
    simulator: &Simulator,
    auction: &competition::Auction,
) -> Result<Score, Error> {
    let solution = solver.solve(auction).await?;
    let gas = solution.simulate(eth, simulator, auction).await?;
    let settlement = Settlement::encode(eth, auction, &solution).await?;
    settlement
        .score(eth, auction, gas)
        .await
        .map_err(Into::into)
}

// TODO This is ready for documenting
/// A unique solution ID. TODO Once this is finally decided, document what this
/// ID is used for.
#[derive(Debug, Clone, Copy)]
pub struct Id(pub u32);

impl Id {
    pub fn random() -> Self {
        Self(rand::thread_rng().gen())
    }

    pub fn to_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("solver error: {0:?}")]
    Solver(#[from] solver::Error),
    #[error("simulation error: {0:?}")]
    Simulation(#[from] simulator::Error),
    #[error("blockchain error: {0:?}")]
    Blockchain(#[from] blockchain::Error),
    #[error("boundary error: {0:?}")]
    Boundary(#[from] boundary::Error),
}
