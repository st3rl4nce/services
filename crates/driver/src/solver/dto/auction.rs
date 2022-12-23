use {
    crate::{logic::competition, util::serialize},
    primitive_types::{H160, U256},
    serde::Serialize,
    serde_with::serde_as,
    std::collections::HashMap,
};

// TODO Since building the auction will also require liquidity later down the
// line, this is probably not good enough. But that will be implemented when the
// `logic::liquidity` module is added.
impl Auction {
    pub fn new(
        _auction: &competition::Auction,
        _deadline: &competition::auction::SolverDeadline,
    ) -> Self {
        todo!()
    }
}

#[serde_as]
#[derive(Debug, Serialize)]
pub struct Auction {
    id: Option<String>,
    tokens: HashMap<H160, Token>,
    orders: Vec<Order>,
    liquidity: Vec<Liquidity>,
    #[serde_as(as = "serialize::U256")]
    effective_gas_price: U256,
    deadline: chrono::DateTime<chrono::Utc>,
}

#[serde_as]
#[derive(Debug, Serialize)]
struct Order {
    #[serde_as(as = "serialize::Hex")]
    uid: [u8; 56],
    sell_token: H160,
    buy_token: H160,
    #[serde_as(as = "serialize::U256")]
    sell_amount: U256,
    #[serde_as(as = "serialize::U256")]
    buy_amount: U256,
    #[serde_as(as = "serialize::U256")]
    fee_amount: U256,
    kind: Kind,
    partially_fillable: bool,
    class: Class,
    reward: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Sell,
    Buy,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Class {
    Market,
    Limit,
    Liquidity,
}

#[serde_as]
#[derive(Debug, Serialize)]
struct Token {
    decimals: Option<u8>,
    symbol: Option<String>,
    reference_price: Option<f64>,
    #[serde_as(as = "serialize::U256")]
    available_balance: U256,
    trusted: bool,
}

#[serde_as]
#[derive(Debug, Serialize)]
struct Liquidity {
    id: String,
    address: H160,
    #[serde_as(as = "serialize::U256")]
    gas_estimate: U256,
}
