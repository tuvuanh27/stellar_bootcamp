use soroban_sdk::{contract, contractimpl, contracttype, token, Address, Env, Map};

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Owner,
    Pool(Address),
    UserPos(Address),
    Ltv(Address),
    Price(Address),
}

/// Represents a single asset pool.
#[derive(Clone, Debug)]
#[contracttype]
pub struct Pool {
    pub token: Address,
    pub total_supply_shares: i128,
    pub total_debt_shares: i128,
    pub total_reserves: i128,
}

#[derive(Clone, Debug)]
#[contracttype]
pub struct UserPosition {
    /// Map<Token Address, Amount Deposited>
    pub deposit_shares: Map<Address, i128>,
    /// Map<Token Address, Amount Borrowed>
    pub debt_shares: Map<Address, i128>,
}

#[contract]
pub struct LendingPool;

#[contractimpl]
impl LendingPool {
    // --- Constructor & Admin ---

    /// Initializes the contract and sets the administrator.
    pub fn __constructor(e: Env, admin: Address) {
        // Use persistent storage for the owner.
        // `instance` storage is loaded on every call and best for
        // data needed *every time*. Persistent is better for most data.
        e.storage().persistent().set(&DataKey::Owner, &admin);
    }

    /// (Admin) Adds a new token to be used as a lending pool.
    pub fn init_pool(e: Env, token: Address) {
        Self::get_owner(&e).require_auth();

        let key = DataKey::Pool(token.clone());
        if e.storage().persistent().has(&key) {
            panic!("pool already initialized");
        }

        // NOTE: Manually construct `Pool` instead of using `..Default`.
        // We must clone `token` here because it's not `Copy` and is moved.
        e.storage().persistent().set(
            &key,
            &Pool {
                token: token.clone(), // This clone fixes the move error.
                total_supply_shares: 0,
                total_debt_shares: 0,
                total_reserves: 0,
            },
        );

        // Initialize LTV and Price to 0
        e.storage()
            .persistent()
            .set(&DataKey::Ltv(token.clone()), &0u32);
        e.storage().persistent().set(&DataKey::Price(token), &0i128); // `token` is moved here.
    }

    /// (Admin) Sets the Loan-To-Value ratio for an asset.
    /// LTV is a number out of 10,000 (e.g., 7500 = 75%).
    pub fn set_ltv(e: Env, token: Address, ltv: u32) {
        Self::get_owner(&e).require_auth();
        if ltv > 10000 {
            panic!("LTV cannot be over 10000");
        }
        e.storage().persistent().set(&DataKey::Ltv(token), &ltv);
    }

    /// (Admin) Sets the mock price for an asset.
    /// Price should have a consistent number of decimals, e.g., 7.
    /// (e.g., $1.50 = 15000000)
    pub fn set_price(e: Env, token: Address, price: i128) {
        Self::get_owner(&e).require_auth();
        if price < 0 {
            panic!("price cannot be negative");
        }
        e.storage().persistent().set(&DataKey::Price(token), &price);
    }

    // --- Core Functions ---

    /// Supplies assets to a pool.
    /// `user` deposits `amount` of `token` into the contract.
    pub fn supply(e: Env, user: Address, token: Address, amount: i128) {
        user.require_auth();
        if amount <= 0 {
            panic!("amount must be positive");
        }

        // Get persistent state
        let mut pool = Self::get_pool(&e, token.clone());
        let mut user_pos = Self::get_user_pos(&e, user.clone());

        // 1. Transfer tokens from user to this contract
        let token_client = token::Client::new(&e, &token);
        token_client.transfer(&user, &e.current_contract_address(), &amount);

        // 2. Update user's deposit balance
        let new_deposits = user_pos.deposit_shares.get(token.clone()).unwrap_or(0) + amount;
        user_pos.deposit_shares.set(token.clone(), new_deposits);

        // 3. Update pool's total supply
        pool.total_supply_shares += amount;

        // 4. Save the updated state
        Self::save_pool(&e, token, &pool); // `token` is moved here
        Self::save_user_pos(e, user, &user_pos); // `user` is moved here
    }

    /// Withdraws assets from a pool.
    /// `user` withdraws `amount` of `token` from the contract.
    pub fn withdraw(e: Env, user: Address, token: Address, amount: i128) {
        user.require_auth();
        if amount <= 0 {
            panic!("amount must be positive");
        }

        // Get persistent state
        let mut pool = Self::get_pool(&e, token.clone());
        let mut user_pos = Self::get_user_pos(&e, user.clone());

        // 1. Check user's current deposit
        let current_deposit = user_pos.deposit_shares.get(token.clone()).unwrap_or(0);
        if current_deposit == 0 {
            panic!("no assets to withdraw");
        }

        // 2. Determine actual amount to withdraw (can't over-withdraw)
        let amount_to_withdraw = amount.min(current_deposit);

        user_pos
            .deposit_shares
            .set(token.clone(), current_deposit - amount_to_withdraw);
        let (collateral_value, debt_value) = Self::get_user_health(&e, &user_pos);

        if collateral_value < debt_value {
            // Revert state before panicking
            user_pos.deposit_shares.set(token.clone(), current_deposit);
            panic!("insufficient collateral after withdrawal");
        }

        // 4. Check for available liquidity in the pool
        let available_liquidity = pool.total_supply_shares - pool.total_debt_shares;
        if amount_to_withdraw > available_liquidity {
            panic!("insufficient liquidity in the pool");
        }

        // 5. Update pool's total supply
        pool.total_supply_shares -= amount_to_withdraw;

        // 6. Transfer tokens from this contract to the user
        let token_client = token::Client::new(&e, &token);
        token_client.transfer(&e.current_contract_address(), &user, &amount_to_withdraw);

        // 7. Save the updated state (user_pos is already updated)
        Self::save_pool(&e, token, &pool);
        Self::save_user_pos(e, user, &user_pos);
    }

    /// Repays a debt.
    /// `user` repays `amount` of `token` to the contract.
    pub fn repay(e: Env, user: Address, token: Address, amount: i128) {
        user.require_auth();
        if amount <= 0 {
            panic!("amount must be positive");
        }

        // Get persistent state
        let mut pool = Self::get_pool(&e, token.clone());
        let mut user_pos = Self::get_user_pos(&e, user.clone());

        // 1. Check user's current debt
        let current_debt = user_pos.debt_shares.get(token.clone()).unwrap_or(0);
        if current_debt == 0 {
            panic!("no debt to repay");
        }

        // 2. Determine actual amount to repay (can't overpay)
        let amount_to_repay = amount.min(current_debt);

        // 3. Transfer tokens from user to this contract
        let token_client = token::Client::new(&e, &token);
        token_client.transfer(&user, &e.current_contract_address(), &amount_to_repay);

        // 4. Update user's debt balance
        user_pos
            .debt_shares
            .set(token.clone(), current_debt - amount_to_repay);

        // 5. Update pool's total debt
        pool.total_debt_shares -= amount_to_repay;

        // 6. Save the updated state
        Self::save_pool(&e, token, &pool);
        Self::save_user_pos(e, user, &user_pos);
    }

    /// Borrows assets from a pool.
    pub fn borrow(e: Env, user: Address, token: Address, amount: i128) {
        user.require_auth();
        if amount <= 0 {
            panic!("amount must be positive");
        }

        let mut user_pos = Self::get_user_pos(&e, user.clone());
        let (collateral_value, mut debt_value) = Self::get_user_health(&e, &user_pos);

        // 2. Calculate new debt value
        let price = Self::get_price(&e, token.clone());
        if price <= 0 {
            panic!("price for this asset is not set");
        }
        let new_debt_value = amount.checked_mul(price).expect("overflow");
        debt_value += new_debt_value;

        // 3. Check if health factor is safe
        //    (Total Collateral Value must be >= Total Debt Value)
        if collateral_value < debt_value {
            panic!("insufficient collateral");
        }

        // 4. Get persistent pool state
        let mut pool = Self::get_pool(&e, token.clone());

        // 5. Check for available liquidity in the pool
        // (Reserves are not implemented, so just total supply - total debt)
        let available_liquidity = pool.total_supply_shares - pool.total_debt_shares;
        if amount > available_liquidity {
            panic!("insufficient liquidity in the pool");
        }

        // 6. Update user's debt
        let new_debt = user_pos.debt_shares.get(token.clone()).unwrap_or(0) + amount;
        user_pos.debt_shares.set(token.clone(), new_debt);

        // 7. Update pool's total debt
        pool.total_debt_shares += amount;

        // 8. Transfer tokens from this contract to the user
        let token_client = token::Client::new(&e, &token);
        token_client.transfer(&e.current_contract_address(), &user, &amount);

        // 9. Save the updated state
        Self::save_pool(&e, token, &pool);
        Self::save_user_pos(e, user, &user_pos);
    }

    // --- Helper Functions ---
    fn get_owner(e: &Env) -> Address {
        e.storage()
            .persistent()
            .get(&DataKey::Owner)
            .expect("owner not set")
    }

    /// Gets the `Pool` struct for a given `token`.
    fn get_pool(e: &Env, token: Address) -> Pool {
        e.storage()
            .persistent()
            .get(&DataKey::Pool(token))
            .expect("pool not initialized")
    }

    /// Saves the `Pool` struct for a given `token`.
    fn save_pool(e: &Env, token: Address, pool: &Pool) {
        e.storage().persistent().set(&DataKey::Pool(token), pool);
    }

    /// Gets the `UserPosition` struct for a given `user`.
    /// Returns a new, empty struct if this is a new user.
    fn get_user_pos(e: &Env, user: Address) -> UserPosition {
        e.storage()
            .persistent()
            .get(&DataKey::UserPos(user))
            .unwrap_or_else(|| UserPosition {
                deposit_shares: Map::new(e),
                debt_shares: Map::new(e),
            })
    }

    fn save_user_pos(e: Env, user: Address, pos: &UserPosition) {
        e.storage().persistent().set(&DataKey::UserPos(user), pos);
    }

    fn get_ltv(e: &Env, token: Address) -> u32 {
        e.storage()
            .persistent()
            .get(&DataKey::Ltv(token))
            .unwrap_or(0)
    }

    fn get_price(e: &Env, token: Address) -> i128 {
        e.storage()
            .persistent()
            .get(&DataKey::Price(token))
            .unwrap_or(0)
    }

    fn get_user_health(e: &Env, user_pos: &UserPosition) -> (i128, i128) {
        let mut total_collateral_value: i128 = 0;
        let mut total_debt_value: i128 = 0;

        for (token, amount) in user_pos.deposit_shares.iter() {
            let price = Self::get_price(e, token.clone());
            let ltv = Self::get_ltv(e, token);
            let value = amount.checked_mul(price).expect("overflow");
            let collateral_value = value
                .checked_mul(ltv as i128)
                .expect("overflow")
                .checked_div(10000)
                .expect("div by zero");
            total_collateral_value += collateral_value;
        }

        // Calculate total debt value
        for (token, amount) in user_pos.debt_shares.iter() {
            let price = Self::get_price(e, token);
            let debt_value = amount.checked_mul(price).expect("overflow");
            total_debt_value += debt_value;
        }

        (total_collateral_value, total_debt_value)
    }
}
