use expanduser::expanduser;
use openssl::{
    ec, pkey,
    pkey::{Private, Public},
    sha::sha256,
};
use rusqlite as sql;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
};
use thiserror::Error;

// Constants

pub const WALLET_PATH: &str = "~/.config/rs_simple_blockchain/wallet.pem";

pub const MINIMUM_DIFFICULTY_LEVEL: u8 = 12;

// Types

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Amount(u64);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hash([u8; 32]);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayerPublicKey(Vec<u8>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature(Vec<u8>);

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionInput {
    transaction_hash: Hash,
    output_index: u16,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionOutput {
    amount: Amount,
    recipient_hash: Hash,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Transaction {
    payer: PayerPublicKey,
    inputs: Vec<TransactionInput>,
    outputs: Vec<TransactionOutput>,
    signature: Signature,
    transaction_hash: Hash,
}

#[derive(Debug, Clone)]
pub struct Wallet {
    public_serialized: PayerPublicKey,
    public_hash: Hash,
    private_key: ec::EcKey<Private>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    nonce: u64,
    transactions: Vec<Transaction>,
    parent_hash: Option<Hash>,
    block_hash: Hash,
}

#[derive(Debug)]
pub struct BlockchainStorage {
    path: Option<std::path::PathBuf>,
    conn: sql::Connection,
    default_wallet: Wallet,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockchainStats {
    pub block_count: u64,
    pub pending_txn_count: u64,
}

#[derive(Error, Debug)]
pub enum BlockchainError {
    #[error("transaction is invalid: {0}")]
    InvalidTxn(&'static str),
    #[error("received block is invalid: {0}")]
    InvalidReceivedBlock(&'static str),
    #[error("the tentative transaction is invalid: {0:?}")]
    InvalidTentativeTxn(std::collections::HashMap<Hash, &'static str>),
    #[error("insufficient balance: requested {requested_amount} has {available_amount}")]
    InsufficientBalance { requested_amount: Amount, available_amount: Amount },
    #[error("the monetary amount is too large: amount {0} exceeds maximum representable amount {}", Amount::MAX_MONEY.0)]
    MonetaryAmountTooLarge(u64),
}

// Impls

impl Amount {
    const COIN: Amount = Amount(1_0000_0000);
    const BLOCK_REWARD: Amount = Amount(10 * Amount::COIN.0);
    const MAX_MONEY: Amount = Amount(100_000_000_000 * Amount::COIN.0);
}

impl std::convert::TryFrom<u64> for Amount {
    type Error = BlockchainError;
    fn try_from(u: u64) -> Result<Amount, BlockchainError> {
        if u > Amount::MAX_MONEY.0 {
            Err(BlockchainError::MonetaryAmountTooLarge(u))
        } else {
            Ok(Amount(u))
        }
    }
}

impl std::ops::Mul<u64> for Amount {
    type Output = Self;
    fn mul(self, rhs: u64) -> Self {
        debug_assert!(self.0.checked_mul(rhs).map_or(false, |a| a <= Amount::MAX_MONEY.0));
        Amount(self.0 * rhs)
    }
}

impl sql::ToSql for Amount {
    fn to_sql(self: &Self) -> sql::Result<sql::types::ToSqlOutput> {
        // NOTE that the maximum amount of money can be expressed as i64.
        debug_assert!(self.0 <= (i64::max_value() as u64));
        Ok((self.0 as i64).into())
    }
}

impl sql::types::FromSql for Amount {
    fn column_result(value: sql::types::ValueRef) -> sql::types::FromSqlResult<Self> {
        let r: sql::types::FromSqlResult<i64> = sql::types::FromSql::column_result(value);
        r.map(|v| Amount(v as u64))
    }
}

impl std::fmt::Display for Amount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let integral_part = self.0 / Amount::COIN.0;
        let fractional_part = self.0 % Amount::COIN.0;
        let integral_part_w_sep = Vec::from(format!("{}", integral_part))
            .rchunks(3)
            .rfold(None, |r, c| match r {
                None => Some(c.to_owned()),
                Some(mut rc) => Some({
                    rc.push(b',');
                    rc.extend_from_slice(c);
                    rc
                }),
            })
            .unwrap();
        write!(f, "{}.{:08}", unsafe { String::from_utf8_unchecked(integral_part_w_sep) }, fractional_part)
    }
}

impl Hash {
    pub fn zeroes() -> Self { Hash([0u8; 32]) }

    pub fn sha256(b: &[u8]) -> Self { Hash(sha256(b)) }

    pub fn has_difficulty(self: &Self, mut difficulty: u8) -> bool {
        for &byte in self.0.iter() {
            if difficulty == 0 {
                return true;
            } else if difficulty < 8 {
                return byte.leading_zeros() >= difficulty.into();
            } else {
                if byte != 0 {
                    return false;
                }
                difficulty -= 8;
            }
        }
        // NOTE that the u8 type is carefully chosen because it can't represent
        // any number greater than or equal to 256, or 32*8. Worst case
        // scenario, when difficulty is 255, in the last iteration of the loop
        // difficulty would be < 8 and therefore return.
        unreachable!()
    }

    pub fn display_base58(self: &Self) -> String { bs58::encode(&self.0).into_string() }

    pub fn display_hex(self: &Self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        for &b in self.0.iter() {
            write!(&mut s, "{:x}", b).unwrap();
        }
        s
    }
}

impl sql::ToSql for Hash {
    fn to_sql(self: &Self) -> sql::Result<sql::types::ToSqlOutput> { (&self.0[..]).to_sql() }
}

impl sql::types::FromSql for Hash {
    fn column_result(value: sql::types::ValueRef) -> sql::types::FromSqlResult<Self> {
        let val: Vec<u8> = sql::types::FromSql::column_result(value)?;
        if val.len() == 32 {
            let mut arr = [0; 32];
            arr.copy_from_slice(&val[..32]);
            Ok(Hash(arr))
        } else {
            Err(sql::types::FromSqlError::InvalidType)
        }
    }
}

impl PayerPublicKey {
    fn check_len(self: &Self) -> bool { self.0.len() == 88 }
}

impl sql::ToSql for PayerPublicKey {
    fn to_sql(self: &Self) -> sql::Result<sql::types::ToSqlOutput> { (&self.0[..]).to_sql() }
}

impl sql::types::FromSql for PayerPublicKey {
    fn column_result(value: sql::types::ValueRef) -> sql::types::FromSqlResult<Self> {
        sql::types::FromSql::column_result(value).map(PayerPublicKey)
    }
}

impl sql::ToSql for Signature {
    fn to_sql(self: &Self) -> sql::Result<sql::types::ToSqlOutput> { (&self.0[..]).to_sql() }
}

impl sql::types::FromSql for Signature {
    fn column_result(value: sql::types::ValueRef) -> sql::types::FromSqlResult<Self> {
        sql::types::FromSql::column_result(value).map(Signature)
    }
}

impl Transaction {
    fn recalc_hash(self: &mut Self) {
        let transaction_hash = Hash::sha256(self.signature.0.as_slice());
        self.transaction_hash = transaction_hash;
    }

    fn to_signature_data(self: &Self) -> Vec<u8> {
        let content = (&self.payer, &self.inputs, &self.outputs);
        bincode::serialize(&content).unwrap()
    }

    pub fn transaction_hash(self: &Self) -> &Hash { &self.transaction_hash }

    pub fn verify_signature(self: &Self) -> bool {
        fn verify(t: &Transaction) -> Result<bool, openssl::error::ErrorStack> {
            let pubkey = pkey::PKey::public_key_from_der(t.payer.0.as_slice())?;
            let eckey = pubkey.ec_key()?;
            let sig = openssl::ecdsa::EcdsaSig::from_der(&t.signature.0)?;
            sig.verify(&sha256(t.to_signature_data().as_slice()), &eckey)
        }
        self.payer.check_len() && verify(self).unwrap_or(false)
    }
}

impl serde::Serialize for Transaction {
    fn serialize<S: serde::Serializer>(self: &Self, se: S) -> Result<S::Ok, S::Error> {
        (&self.payer, &self.inputs, &self.outputs, &self.signature).serialize(se)
    }
}

impl<'de> serde::Deserialize<'de> for Transaction {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        type Inner = (PayerPublicKey, Vec<TransactionInput>, Vec<TransactionOutput>, Signature);
        Inner::deserialize(de).map(|(payer, inputs, outputs, signature)| {
            let transaction_hash = Hash::sha256(signature.0.as_slice());
            Transaction { payer, inputs, outputs, signature, transaction_hash }
        })
    }
}

impl PartialEq for Wallet {
    fn eq(&self, other: &Self) -> bool { self.public_serialized == other.public_serialized }
}

impl Wallet {
    fn from_privkey(privkey: ec::EcKey<Private>) -> Result<Self, openssl::error::ErrorStack> {
        privkey.check_key()?;
        let ecg = privkey.group();
        let correct_type = ecg.curve_name().map_or(false, |nid| nid == openssl::nid::Nid::SECP256K1);
        assert!(correct_type);
        let pubkey: ec::EcKey<Public> = ec::EcKey::from_public_key(ecg, privkey.public_key())?;
        let public_serialized = PayerPublicKey(pkey::PKey::from_ec_key(pubkey)?.public_key_to_der()?);
        let public_hash = Hash::sha256(&public_serialized.0);
        Ok(Wallet { private_key: privkey, public_serialized, public_hash })
    }

    pub fn new() -> Self {
        let ecg = ec::EcGroup::from_curve_name(openssl::nid::Nid::SECP256K1).unwrap();
        let privkey = ec::EcKey::generate(ecg.as_ref()).unwrap();
        Wallet::from_privkey(privkey).unwrap()
    }

    pub fn public_key_hash(self: &Self) -> &Hash { &self.public_hash }

    fn create_raw_transaction(
        self: &Self, inputs: Vec<TransactionInput>, outputs: Vec<TransactionOutput>,
    ) -> Transaction {
        assert!(inputs.len() < 256);
        assert!(outputs.len() < 256);
        let mut txn = Transaction {
            payer: self.public_serialized.clone(),
            inputs,
            outputs,
            signature: Signature(vec![]),
            transaction_hash: Hash::zeroes(),
        };
        let sig =
            openssl::ecdsa::EcdsaSig::sign(&sha256(txn.to_signature_data().as_slice()), &self.private_key).unwrap();
        let sig_der = sig.to_der().unwrap();
        txn.signature = Signature(sig_der);
        assert!(txn.verify_signature(), "newly created signature should be verified");
        txn.recalc_hash();
        txn
    }

    fn save_to_disk(self: &Self) -> std::io::Result<()> {
        let pem = self.private_key.private_key_to_pem().unwrap();
        let path = expanduser(WALLET_PATH)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut f = File::create(path)?;
        f.write_all(pem.as_slice())
    }

    fn load_from_disk() -> Option<Self> {
        fn read() -> std::io::Result<Vec<u8>> {
            let mut f = File::open(expanduser(WALLET_PATH)?)?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            Ok(buf)
        }
        fn des(buf: Vec<u8>) -> Result<Wallet, openssl::error::ErrorStack> {
            let eckey = ec::EcKey::private_key_from_pem(buf.as_slice())?;
            Wallet::from_privkey(eckey)
        }
        read().ok().and_then(|buf| des(buf).ok())
    }
}

impl Block {
    fn to_hash_challenge(self: &Self) -> Vec<u8> {
        let content = (&self.nonce, &self.transactions, &self.parent_hash);
        bincode::serialize(&content).unwrap()
    }

    pub fn solve_hash_challenge(self: &mut Self, difficulty: u8, max_tries: Option<u64>) -> bool {
        let mut b = self.to_hash_challenge();
        for _ in 0..max_tries.unwrap_or(1 << 63) {
            let this_hash = Hash::sha256(&b);
            if this_hash.has_difficulty(difficulty) {
                self.block_hash = this_hash;
                return true;
            }
            self.nonce += 1;
            self.nonce %= 1 << 63;
            bincode::serialize_into(&mut b[0..8], &self.nonce).unwrap();
            debug_assert_eq!(b, self.to_hash_challenge());
        }
        false
    }

    pub fn verify_hash_challenge(self: &Self, difficulty: u8) -> bool {
        self.block_hash.has_difficulty(difficulty) && self.block_hash == Hash::sha256(&self.to_hash_challenge())
    }

    fn new_mine_block(w: &Wallet) -> Self {
        Block {
            parent_hash: None,
            block_hash: Hash::zeroes(),
            nonce: 0,
            transactions: vec![w.create_raw_transaction(vec![], vec![TransactionOutput {
                recipient_hash: Hash::sha256(&w.public_serialized.0),
                amount: Amount::BLOCK_REWARD,
            }])],
        }
    }
}

macro_rules! replace_expr {
    ($_t:tt $sub:expr) => {
        $sub
    };
}

macro_rules! execute {
    ( $t:expr, $sql:expr ) => {
        execute!($t, $sql, )
    };
    ( $t:expr, $sql:expr, $( $param:expr ),* ) => {
        {
            let mut stmt = $t.prepare_cached($sql)?;
            let params: [&dyn sql::ToSql; {0usize $(+ replace_expr!($param 1usize))* }] = [ $( $param ),* ];
            stmt.execute(&params)
        }
    }
}

macro_rules! query_row {
    ( $t:expr, $sql:expr ; $( $rv:ident : $rt:ty ),+ ; $re:expr ) => {
        query_row!($t, $sql, ; $( $rv : $rt ),+ ; $re)
    };
    ( $t:expr, $sql:expr, $( $param:expr ),* ; $( $rv:ident : $rt:ty ),+ ; $re:expr ) => {
        {
            let mut stmt = $t.prepare_cached($sql)?;
            let params: [&dyn sql::ToSql; {0usize $(+ replace_expr!($param 1usize))* }] = [ $( $param ),* ];
            stmt.query_row(&params, |row| {
                let mut idx: usize = 0;
                $( let $rv = { idx += 1; row.get_unwrap::<usize, $rt>(idx - 1) }  );* ;
                Ok($re)
            })
        }
    }
}

macro_rules! query_vec {
    ( $t:expr, $sql:expr ; $( $rv:ident : $rt:ty ),+ ; $re:expr ) => {
        query_vec!($t, $sql, ; $( $rv : $rt ),+ ; $re)
    };
    ( $t:expr, $sql:expr, $( $param:expr ),* ; $( $rv:ident : $rt:ty ),+ ; $re:expr ) => {
        {
            let mut stmt = $t.prepare_cached($sql)?;
            let params: [&dyn sql::ToSql; {0usize $(+ replace_expr!($param 1usize))* }] = [ $( $param ),* ];
            let rows = stmt.query_map(&params, |row| {
                let mut idx: usize = 0;
                $( let $rv = { idx += 1; row.get_unwrap::<usize, $rt>(idx - 1) }  );* ;
                Ok($re)
            })?;
            rows.collect::<sql::Result<Vec<_>>>()
        }
    }
}

impl BlockchainStorage {
    fn open_conn(path: Option<&std::path::Path>) -> sql::Connection {
        let conn = match path {
            None => sql::Connection::open_in_memory().unwrap(),
            Some(ref p) => sql::Connection::open(p).unwrap(),
        };
        assert!(conn.is_autocommit());
        conn.set_prepared_statement_cache_capacity(64);
        conn.execute_batch(
            "
                PRAGMA foreign_keys = ON;
                PRAGMA journal_mode = WAL;
                CREATE TABLE IF NOT EXISTS blocks (
                    block_hash BLOB NOT NULL PRIMARY KEY ON CONFLICT IGNORE,
                    parent_hash BLOB REFERENCES blocks (block_hash),
                    block_height INTEGER NOT NULL DEFAULT 0,
                    nonce INTEGER NOT NULL,
                    discovered_at REAL NOT NULL DEFAULT ((julianday('now') - 2440587.5)*86400.0),
                    CHECK ( block_height >= 0 ),
                    CHECK ( nonce >= 0 ),
                    CHECK ( length(block_hash) = 32 OR block_hash = x'deadface' )
                );
                CREATE INDEX IF NOT EXISTS block_parent ON blocks (parent_hash);
                CREATE INDEX IF NOT EXISTS block_height ON blocks (block_height);
                CREATE INDEX IF NOT EXISTS block_discovered_at ON blocks (discovered_at);
                CREATE TRIGGER IF NOT EXISTS set_block_height
                AFTER INSERT ON blocks
                FOR EACH ROW BEGIN
                    UPDATE blocks
                    SET block_height = (SELECT ifnull((SELECT 1 + block_height FROM blocks WHERE block_hash = NEW.parent_hash), 0))
                    WHERE block_hash = NEW.block_hash;
                END;

                CREATE TABLE IF NOT EXISTS transactions (
                    transaction_hash BLOB NOT NULL PRIMARY KEY ON CONFLICT IGNORE,
                    payer BLOB NOT NULL,
                    payer_hash BLOB NOT NULL,
                    discovered_at REAL NOT NULL DEFAULT ((julianday('now') - 2440587.5)*86400.0),
                    signature BLOB NOT NULL,
                    CHECK ( length(transaction_hash) = 32 ),
                    CHECK ( length(payer) = 88 ),
                    CHECK ( length(payer_hash) = 32 )
                );
                CREATE INDEX IF NOT EXISTS transaction_payer ON transactions (payer_hash);

                CREATE TABLE IF NOT EXISTS transaction_in_block (
                    transaction_hash BLOB NOT NULL REFERENCES transactions,
                    block_hash BLOB NOT NULL REFERENCES blocks ON DELETE CASCADE,
                    transaction_index INTEGER NOT NULL,
                    UNIQUE (transaction_hash, block_hash),
                    UNIQUE (block_hash, transaction_index),
                    CHECK ( transaction_index BETWEEN 0 AND 1999 )
                );

                CREATE TABLE IF NOT EXISTS transaction_outputs (
                    out_transaction_hash BLOB NOT NULL REFERENCES transactions (transaction_hash),
                    out_transaction_index INTEGER NOT NULL,
                    amount INTEGER NOT NULL,
                    recipient_hash BLOB NOT NULL,
                    PRIMARY KEY (out_transaction_hash, out_transaction_index) ON CONFLICT IGNORE,
                    UNIQUE (out_transaction_hash, recipient_hash),
                    CHECK ( amount > 0 ),
                    CHECK ( out_transaction_index BETWEEN 0 AND 255 ),
                    CHECK ( length(recipient_hash) = 32 )
                );
                CREATE INDEX IF NOT EXISTS output_recipient ON transaction_outputs (recipient_hash);

                CREATE TABLE IF NOT EXISTS transaction_inputs (
                    in_transaction_hash BLOB NOT NULL REFERENCES transactions (transaction_hash),
                    in_transaction_index INTEGER NOT NULL,
                    out_transaction_hash BLOB NOT NULL,
                    out_transaction_index INTEGER NOT NULL,
                    PRIMARY KEY (in_transaction_hash, in_transaction_index) ON CONFLICT IGNORE,
                    FOREIGN KEY(out_transaction_hash, out_transaction_index) REFERENCES transaction_outputs,
                    CHECK ( in_transaction_index BETWEEN 0 AND 255 )
                );
                CREATE INDEX IF NOT EXISTS input_referred ON transaction_inputs (out_transaction_hash, out_transaction_index);

                CREATE TABLE IF NOT EXISTS trustworthy_wallets (
                    payer_hash BLOB NOT NULL PRIMARY KEY ON CONFLICT IGNORE,
                    CHECK ( length(payer_hash) = 32 )
                );

                CREATE TABLE IF NOT EXISTS orphaned_transactions (
                    transaction_hash BLOB NOT NULL PRIMARY KEY ON CONFLICT IGNORE,
                    transaction_blob BLOB NOT NULL,
                    CHECK ( length(transaction_hash) = 32 )
                );

                CREATE TABLE IF NOT EXISTS orphaned_transactions_missing_deps (
                    transaction_hash BLOB NOT NULL REFERENCES orphaned_transactions (transaction_hash),
                    dependency BLOB NOT NULL,
                    PRIMARY KEY (transaction_hash, dependency) ON CONFLICT IGNORE,
                    CHECK ( length(dependency) = 32 )
                );
                CREATE INDEX IF NOT EXISTS orhpaned_deps ON orphaned_transactions_missing_deps (dependency);

                CREATE VIEW IF NOT EXISTS unauthorized_spending AS
                SELECT transactions.*, transaction_outputs.recipient_hash AS owner_hash, transaction_outputs.amount
                FROM transactions
                JOIN transaction_inputs ON transactions.transaction_hash = transaction_inputs.in_transaction_hash
                JOIN transaction_outputs USING (out_transaction_hash, out_transaction_index)
                WHERE payer_hash != owner_hash;

                CREATE VIEW IF NOT EXISTS transaction_credit_debit AS
                WITH
                transaction_debits AS (
                    SELECT out_transaction_hash AS transaction_hash, sum(amount) AS debited_amount
                    FROM transaction_outputs
                    GROUP BY transaction_hash
                ),
                transaction_credits AS (
                    SELECT in_transaction_hash AS transaction_hash, sum(transaction_outputs.amount) AS credited_amount
                    FROM transaction_inputs JOIN transaction_outputs USING (out_transaction_hash, out_transaction_index)
                    GROUP BY transaction_hash
                )
                SELECT * FROM transaction_credits
                JOIN transaction_debits USING (transaction_hash)
                JOIN transactions USING (transaction_hash);

                CREATE VIEW IF NOT EXISTS ancestors AS
                WITH RECURSIVE
                ancestors AS (
                    SELECT block_hash, block_hash AS ancestor, 0 AS path_length FROM blocks
                    UNION ALL
                    SELECT ancestors.block_hash, blocks.parent_hash AS ancestor, 1 + path_length AS path_length
                    FROM ancestors JOIN blocks ON ancestor = blocks.block_hash
                    WHERE blocks.parent_hash IS NOT NULL
                )
                SELECT * FROM ancestors;

                CREATE VIEW IF NOT EXISTS longest_chain AS
                WITH RECURSIVE
                initial AS (SELECT * FROM blocks ORDER BY block_height DESC, discovered_at ASC LIMIT 1),
                chain AS (
                    SELECT block_hash, parent_hash, block_height, 1 AS confirmations FROM initial
                    UNION ALL
                    SELECT blocks.block_hash, blocks.parent_hash, blocks.block_height, 1 + confirmations
                        FROM blocks JOIN chain ON blocks.block_hash = chain.parent_hash
                )
                SELECT * FROM chain;

                CREATE VIEW IF NOT EXISTS all_tentative_txns AS
                WITH lc_transaction_in_block AS (
                    SELECT transaction_in_block.* FROM transaction_in_block JOIN longest_chain USING (block_hash)
                ),
                txns_not_on_longest AS (
                    SELECT transaction_hash, payer, signature, discovered_at
                    FROM transactions LEFT JOIN lc_transaction_in_block USING (transaction_hash)
                    WHERE block_hash IS NULL
                )
                SELECT * from txns_not_on_longest WHERE transaction_hash IN (SELECT in_transaction_hash FROM transaction_inputs);

                CREATE VIEW IF NOT EXISTS utxo AS
                WITH tx_confirmations AS (
                    SELECT transaction_in_block.transaction_hash, longest_chain.confirmations
                    FROM transaction_in_block JOIN longest_chain USING (block_hash)
                ),
                all_utxo AS (
                    SELECT transaction_outputs.*
                    FROM transaction_outputs LEFT JOIN transaction_inputs USING (out_transaction_hash, out_transaction_index)
                    WHERE in_transaction_index IS NULL
                ),
                all_utxo_confirmations AS (
                    SELECT all_utxo.*, ifnull(tx_confirmations.confirmations, 0) AS confirmations
                    FROM all_utxo LEFT JOIN tx_confirmations ON all_utxo.out_transaction_hash = tx_confirmations.transaction_hash
                ),
                trustworthy_even_if_unconfirmed AS (
                    SELECT transaction_hash
                    FROM transactions
                    JOIN trustworthy_wallets USING (payer_hash)
                    JOIN transaction_inputs ON transactions.transaction_hash = transaction_inputs.in_transaction_hash
                )
                SELECT *
                FROM all_utxo_confirmations
                WHERE confirmations > 0 OR out_transaction_hash IN (SELECT transaction_hash FROM trustworthy_even_if_unconfirmed);

                CREATE VIEW IF NOT EXISTS block_consistency AS
                SELECT block_hash AS perspective_block, (
                   WITH
                   my_ancestors AS (
                       SELECT ancestor AS block_hash FROM ancestors WHERE block_hash = ob.block_hash
                   ),
                   my_transaction_in_block AS (
                       SELECT transaction_in_block.* FROM transaction_in_block JOIN my_ancestors USING (block_hash)
                   ),
                   my_transaction_inputs AS (
                       SELECT transaction_inputs.*
                       FROM transaction_inputs JOIN my_transaction_in_block
                       ON transaction_inputs.in_transaction_hash = my_transaction_in_block.transaction_hash
                   ),
                   my_transaction_outputs AS (
                       SELECT transaction_outputs.*
                       FROM transaction_outputs JOIN my_transaction_in_block
                       ON transaction_outputs.out_transaction_hash = my_transaction_in_block.transaction_hash
                   ),
                   error_input_referring_to_nonexistent_outputs AS (
                       SELECT count(*) AS violations_count
                       FROM my_transaction_inputs LEFT JOIN my_transaction_outputs USING (out_transaction_hash, out_transaction_index)
                       WHERE my_transaction_outputs.amount IS NULL
                   ),
                   error_double_spent AS (
                       SELECT count(*) AS violations_count FROM (
                           SELECT count(*) AS spent_times
                           FROM my_transaction_outputs JOIN my_transaction_inputs USING (out_transaction_hash, out_transaction_index)
                           GROUP BY out_transaction_hash, out_transaction_index
                           HAVING spent_times > 1
                       )
                   )
                   SELECT (SELECT violations_count FROM error_input_referring_to_nonexistent_outputs) +
                          (SELECT violations_count FROM error_double_spent)
                ) AS total_violations_count
                FROM blocks AS ob;").unwrap();
        conn
    }
    pub fn new(path: Option<&std::path::Path>, default_wallet: Option<&Wallet>) -> Self {
        BlockchainStorage {
            default_wallet: default_wallet.cloned().or_else(Wallet::load_from_disk).unwrap_or_else(|| {
                let w = Wallet::new();
                w.save_to_disk().unwrap();
                w
            }),
            path: path.map(|p| p.to_path_buf()),
            conn: BlockchainStorage::open_conn(path),
        }
    }

    pub fn recreate_db(self: &mut Self) {
        fn unlink_ignore_enoent(p: &std::path::Path) -> std::io::Result<()> {
            std::fs::remove_file(p).or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            })
        }
        fn add(p: &std::path::Path, suffix: &str) -> std::path::PathBuf {
            let mut f = p.file_name().unwrap().to_os_string();
            f.push(suffix);
            p.with_file_name(f)
        }

        // First, drop the database. (There's no "invalid" state for the
        // Connection object so we supply a new, blank connection.)
        std::mem::replace(&mut self.conn, sql::Connection::open_in_memory().unwrap());

        // Then, unlink all files, if needed and present.
        if let Some(ref p) = self.path {
            unlink_ignore_enoent(p).unwrap();
            unlink_ignore_enoent(&add(p, "-shm")).unwrap();
            unlink_ignore_enoent(&add(p, "-wal")).unwrap();
        }

        // Finally, recreate the database on disk.
        std::mem::replace(&mut self.conn, BlockchainStorage::open_conn(self.path.as_deref()));
    }

    pub fn produce_stats(self: &Self) -> sql::Result<BlockchainStats> {
        query_row!(self.conn, "SELECT 1 + ifnull((SELECT max(block_height) FROM blocks), -1), (SELECT count(*) FROM all_tentative_txns)";
                   b: i64, t: i64; BlockchainStats {block_count: b as u64, pending_txn_count: t as u64})
    }

    pub fn make_wallet_trustworthy(self: &Self, h: &Hash) -> sql::Result<()> {
        execute!(self.conn, "INSERT INTO trustworthy_wallets VALUES (?)", h)?;
        Ok(())
    }

    pub fn make_wallet(self: &mut Self) -> sql::Result<Wallet> {
        let w = Wallet::new();
        self.make_wallet_trustworthy(&Hash::sha256(&w.public_serialized.0))?;
        Ok(w)
    }

    fn insert_transaction_raw(
        t: &impl std::ops::Deref<Target = sql::Connection>, txn: &Transaction,
    ) -> anyhow::Result<()> {
        fn report_integrity(e: sql::Error) -> anyhow::Error {
            if let sql::Error::SqliteFailure(
                libsqlite3_sys::Error { code: libsqlite3_sys::ErrorCode::ConstraintViolation, extended_code: ec },
                _,
            ) = e
            {
                BlockchainError::InvalidTxn(libsqlite3_sys::code_to_str(ec)).into()
            } else {
                e.into()
            }
        }

        let txn_hash = txn.transaction_hash();
        let row_count = execute!(
            t,
            "INSERT INTO transactions (transaction_hash, payer, payer_hash, signature) VALUES (?,?,?,?)",
            &txn_hash,
            &txn.payer,
            &Hash::sha256(&txn.payer.0),
            &txn.signature
        )
        .map_err(report_integrity)?;
        if row_count > 0 {
            for (index, out) in txn.outputs.iter().enumerate() {
                execute!(
                    t,
                    "INSERT INTO transaction_outputs VALUES (?,?,?,?)",
                    &txn_hash,
                    &(index as i64),
                    &out.amount,
                    &out.recipient_hash
                )
                .map_err(report_integrity)?;
            }
            for (index, inp) in txn.inputs.iter().enumerate() {
                execute!(
                    t,
                    "INSERT INTO transaction_inputs VALUES (?,?,?,?)",
                    &txn_hash,
                    &(index as i64),
                    &inp.transaction_hash,
                    &inp.output_index
                )
                .map_err(report_integrity)?;
            }
        }
        Ok(())
    }

    pub fn receive_block(self: &mut Self, block: &Block) -> anyhow::Result<()> {
        fn err(msg: &'static str) -> Result<(), BlockchainError> { Err(BlockchainError::InvalidReceivedBlock(msg)) }

        if block.transactions.len() > 2000 {
            err("A block may have at most 2000 transactions")?;
        }

        if block.nonce >= 1 << 63 {
            err("Block nonce must be within 63 bits")?;
        }

        if block.transactions.len() == 0
            || block.transactions[0].inputs.len() != 0
            || block.transactions[0].outputs.len() != 1
            || block.transactions[0].outputs[0].amount != Amount::BLOCK_REWARD
        {
            err("The first transaction must be a reward transaction: have no inputs, and only one output of exactly the reward amount")?;
        }

        if !block.transactions.iter().all(|t| 1 <= t.outputs.len() && t.outputs.len() <= 256) {
            err("Every transaction must have at least one output and at most 256")?;
        }

        if !block.transactions.iter().skip(1).all(|t| 1 <= t.inputs.len() && t.inputs.len() <= 256) {
            err("Every transaction except for the first must have at least one input and at most 256")?;
        }

        if !block.transactions.iter().all(|t| t.outputs.iter().all(|o| o.amount <= Amount::MAX_MONEY)) {
            err("Every output of every transaction must have a value of no more than 100 billion")?;
        }

        if !block.transactions.iter().all(|t| {
            t.outputs.len()
                == t.outputs.iter().map(|o| &o.recipient_hash).collect::<std::collections::HashSet<_>>().len()
        }) {
            err("Every transaction must have distinct output recipients")?;
        }

        if !block.verify_hash_challenge(MINIMUM_DIFFICULTY_LEVEL) {
            err("Block has incorrect or insufficiently hard hash")?;
        }

        if !block.transactions.iter().all(Transaction::verify_signature) {
            err("Every transaction must be correctly signed")?;
        }

        let t = self.conn.transaction()?;

        execute!(
            t,
            "INSERT INTO blocks (block_hash, parent_hash, nonce) VALUES (?,?,?)",
            &block.block_hash,
            &block.parent_hash,
            &(block.nonce as i64)
        )?;
        for txn in block.transactions.iter() {
            BlockchainStorage::insert_transaction_raw(&t, &txn)?;
        }
        for (index, txn) in block.transactions.iter().enumerate() {
            execute!(
                t,
                "INSERT INTO transaction_in_block VALUES (?,?,?)",
                &txn.transaction_hash(),
                &block.block_hash,
                &(index as i64)
            )?;
        }
        if query_row!(t, "SELECT count(*) FROM unauthorized_spending JOIN transaction_in_block USING (transaction_hash) WHERE block_hash = ?",
                      &block.block_hash; r: i64; r > 0)?
        {
            err("Transaction(s) in block contain unauthorized spending")?;
        }
        if query_row!(t,
                      "SELECT count(*) FROM transaction_credit_debit JOIN transaction_in_block USING (transaction_hash) WHERE block_hash = ? AND debited_amount > credited_amount",
                      &block.block_hash; r: i64; r > 0)?
        {
            err("Transaction(s) in block have an input that spends more than the amount in the referenced output")?;
        }
        if query_row!(t,
                      "SELECT total_violations_count FROM block_consistency WHERE perspective_block = ?",
                      &block.block_hash; r: i64; r > 0)?
        {
            err("Transaction(s) in block are not consistent with ancestor blocks; one or more transactions either refer to a nonexistent parent or double spend a previously spent parent")?;
        }

        t.commit()?;
        Ok(())
    }

    fn receive_tentative_transaction_internal(
        t: &impl std::ops::Deref<Target = sql::Connection>, tx: &Transaction,
    ) -> anyhow::Result<()> {
        let th = tx.transaction_hash();

        let err = |msg| Err(BlockchainError::InvalidTentativeTxn(Some((th.clone(), msg)).into_iter().collect()));

        BlockchainStorage::insert_transaction_raw(t, tx).map_err(|e| {
            if let Some(&BlockchainError::InvalidTxn(msg)) = e.downcast_ref::<BlockchainError>() {
                BlockchainError::InvalidTentativeTxn(Some((th.clone(), msg)).into_iter().collect()).into()
            } else {
                e
            }
        })?;

        if query_row!(t, "SELECT count(*) FROM unauthorized_spending WHERE transaction_hash = ?", &th; r: i64; r > 0)? {
            err("The tentative transaction contain unauthorized spending")?;
        }
        if query_row!(t, "SELECT count(*) FROM transaction_credit_debit WHERE transaction_hash = ? AND debited_amount > credited_amount", &th; r: i64; r > 0)?
        {
            err("The tentative transaction has an input that spends more than the amount in the referenced output")?;
        }

        Ok(())
    }

    pub fn receive_tentative_transaction(self: &mut Self, tx: &Transaction) -> anyhow::Result<()> {
        let th = tx.transaction_hash();
        let tx_serialized = bincode::serialize(tx).unwrap();

        let err = |msg| Err(BlockchainError::InvalidTentativeTxn(Some((th.clone(), msg)).into_iter().collect()));

        if !(1 <= tx.outputs.len() && tx.outputs.len() <= 256 && 1 <= tx.inputs.len() && tx.inputs.len() <= 256) {
            err("The tentative transaction must have at least one input and one output, and at most 256")?;
        }

        if !(tx.outputs.iter().all(|o| o.amount <= Amount::MAX_MONEY)) {
            err("Every output of the tentative transaction must have a value of no more than 100 billion")?;
        }

        if tx.outputs.len()
            != tx.outputs.iter().map(|o| &o.recipient_hash).collect::<std::collections::HashSet<_>>().len()
        {
            err("The tentative transaction must have distinct output recipients")?;
        }

        if !tx.verify_signature() {
            err("The tentative transaction must be correctly signed")?;
        }

        let mut t = self.conn.transaction()?;

        // We assume pessimistically that the transaction is orphaned. Later we will (and indeed have to) check this.
        let row_count = execute!(t, "INSERT INTO orphaned_transactions VALUES (?,?)", &th, &tx_serialized)?;
        if row_count > 0 {
            for dep in tx.inputs.iter().map(|i| &i.transaction_hash) {
                execute!(t, "INSERT INTO orphaned_transactions_missing_deps VALUES (?,?)", &th, dep)?;
            }
        }

        BlockchainStorage::collect_orphaned_transactions(&mut t)?;
        t.commit()?;
        Ok(())
    }

    fn collect_orphaned_transactions(t: &mut sql::Transaction) -> anyhow::Result<()> {
        let mut rejected_orphans = std::collections::HashMap::new();
        loop {
            let mut progress = false;
            // Remove all inaccurate dependencies.
            if execute!(t, "DELETE FROM orphaned_transactions_missing_deps WHERE dependency IN (SELECT transaction_hash FROM transactions)")? == 0 {
                break;
            }

            // Find newly de-orphaned transactions
            let adopted = query_vec!(t,
                           "SELECT transaction_hash, transaction_blob FROM orphaned_transactions WHERE transaction_hash NOT IN (SELECT transaction_hash FROM orphaned_transactions_missing_deps)";
                           th: Hash, ts: Vec<u8>;
                           (th, bincode::deserialize(&ts[..]).unwrap()))?;
            for (th, tx) in adopted.into_iter() {
                execute!(t, "DELETE FROM orphaned_transactions WHERE transaction_hash = ?", &th)?;
                let mut sp = t.savepoint()?;
                match BlockchainStorage::receive_tentative_transaction_internal(&sp, &tx) {
                    Ok(()) => {
                        sp.commit()?;
                        progress = true;
                    }
                    Err(mut e) => {
                        if let Some(&mut BlockchainError::InvalidTentativeTxn(ref mut invalid_tx)) =
                            e.downcast_mut::<BlockchainError>()
                        {
                            sp.rollback()?;
                            rejected_orphans.extend(invalid_tx.drain());
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
            if !progress {
                break;
            }
        }
        if rejected_orphans.is_empty() {
            Ok(())
        } else {
            Err(BlockchainError::InvalidTentativeTxn(rejected_orphans).into())
        }
    }

    fn find_available_spend(
        t: &sql::Transaction, wallet_public_key_hash: &Hash,
    ) -> sql::Result<impl Iterator<Item = (TransactionInput, Amount)>> {
        Ok(query_vec!(t, "SELECT out_transaction_hash, out_transaction_index, amount FROM utxo WHERE recipient_hash = ?", wallet_public_key_hash;
                      transaction_hash: Hash, output_index: u16, amt: Amount;
                      (TransactionInput { transaction_hash, output_index }, amt) )?.into_iter()
        )
    }

    pub fn find_wallet_balance(
        self: &Self, wallet_public_key_hash: &Hash, required_confirmations: u32,
    ) -> sql::Result<u64> {
        // NOTE that it is generally incorrect to get UTXO with zero
        // confirmations. They could very well be double-spending transactions
        // that will never get any confirmations. Here we internally make sure
        // that every transaction either has > 0 confirmations or is produced by
        // a trustworthy wallet. Even when the supplied required_confirmations
        // is 0, that invariant is still respected.

        // NOTE that we return a plain u64 because although an individual
        // monetary amount is not allowed to exceed MAX_MONEY, the sum may.
        query_row!(
            self.conn,
            "SELECT sum(amount) FROM utxo WHERE recipient_hash = ? AND confirmations >= ?",
            &wallet_public_key_hash, &required_confirmations;
            s: Option<i64>;
            s.unwrap_or(0) as u64
        )
    }

    pub fn create_simple_transaction(
        self: &mut Self, wallet: Option<&Wallet>, requested_amount: Amount, recipient_hash: &Hash,
    ) -> anyhow::Result<Transaction> {
        let wallet = wallet.unwrap_or(&self.default_wallet);
        let wallet_hash = Hash::sha256(&wallet.public_serialized.0);

        self.make_wallet_trustworthy(&wallet_hash)?; // We have the private key of this wallet so it is trustworthy.

        let t = self.conn.transaction()?;
        let result = BlockchainStorage::find_available_spend(&t, &wallet_hash)?.try_fold(
            (Vec::new(), Amount(0)),
            |(inputs, Amount(sum)), (ti, Amount(amt))| {
                let mut new_inputs = inputs;
                new_inputs.push(ti);
                let rv = (new_inputs, Amount(sum + amt));
                if rv.1 >= requested_amount {
                    Err(rv)
                } else {
                    Ok(rv)
                }
            },
        );
        match result {
            Ok((_, available_amount)) =>
                Err(BlockchainError::InsufficientBalance { available_amount, requested_amount }.into()),
            Err((inputs, total_amount)) => {
                let outputs = if wallet_hash != *recipient_hash {
                    let mut o =
                        vec![TransactionOutput { amount: requested_amount, recipient_hash: recipient_hash.clone() }];
                    if total_amount > requested_amount {
                        o.push(TransactionOutput {
                            amount: Amount(total_amount.0 - requested_amount.0),
                            recipient_hash: wallet_hash,
                        });
                    }
                    o
                } else {
                    vec![TransactionOutput { amount: total_amount, recipient_hash: recipient_hash.clone() }]
                };
                let txn = wallet.create_raw_transaction(inputs, outputs);
                BlockchainStorage::receive_tentative_transaction_internal(&t, &txn)?;
                t.commit()?;
                Ok(txn)
            }
        }
    }

    pub fn get_longest_chain(self: &Self) -> sql::Result<impl Iterator<Item = (Hash, u64)>> {
        Ok(query_vec!(self.conn, "SELECT block_hash, block_height FROM longest_chain"; h: Hash, i: i64; (h, i as u64))?
            .into_iter())
    }

    fn fill_transaction_in_out(
        t: &sql::Transaction, th: Hash, payer: PayerPublicKey, signature: Signature,
    ) -> sql::Result<Transaction> {
        let inputs = query_vec!(t, "SELECT out_transaction_hash, out_transaction_index FROM transaction_inputs WHERE in_transaction_hash = ? ORDER BY in_transaction_index", &th;
                                transaction_hash: Hash, output_index: u16; TransactionInput{transaction_hash, output_index})?;
        let outputs = query_vec!(t, "SELECT amount, recipient_hash FROM transaction_outputs WHERE out_transaction_hash = ? ORDER BY out_transaction_index", &th;
                                 amount: Amount, recipient_hash: Hash; TransactionOutput{amount, recipient_hash})?;
        Ok(Transaction { inputs, outputs, payer, signature, transaction_hash: th })
    }

    pub fn get_block_by_hash(self: &mut Self, block_hash: &Hash) -> sql::Result<Option<Block>> {
        let t = self.conn.transaction()?;
        query_row!(t, "SELECT nonce, parent_hash, block_hash FROM blocks WHERE block_hash = ?", &block_hash; nonce: i64, parent_hash: Option<Hash>, block_hash: Hash; Block {
            nonce: nonce as u64,
            transactions: vec![],
            parent_hash,
            block_hash,
        }).optional()?
        .map_or(Ok(None), |b| {
            Ok(Some(Block {
                transactions: query_vec!(
                    t, "SELECT payer, signature, transaction_hash FROM transactions JOIN transaction_in_block USING (transaction_hash) WHERE block_hash = ? ORDER BY transaction_index", block_hash;
                    p: PayerPublicKey, s: Signature, h: Hash;
                    BlockchainStorage::fill_transaction_in_out(&t, h, p, s)?
                )?,
                ..b
            }))
        })
    }

    pub fn get_all_tentative_transactions(self: &mut Self) -> sql::Result<Vec<Transaction>> {
        let t = self.conn.transaction()?;
        query_vec!(t, "SELECT payer, signature, transaction_hash FROM all_tentative_txns";
                   p: PayerPublicKey, s: Signature, h: Hash;
                   BlockchainStorage::fill_transaction_in_out(&t, h, p, s)?
        )
    }

    pub fn get_mineable_tentative_transactions(
        self: &mut Self, limit: Option<u16>,
    ) -> sql::Result<(Vec<Transaction>, Option<Hash>)> {
        // We need to temporarily modify the database inside the transaction to
        // check for validity. We will not actually make any modifications to
        // the DB.
        let mut t = self.conn.transaction()?;
        let mut rv = Vec::new();
        let limit = limit.unwrap_or(100);

        // Find a parent hash.
        let parent_hash = query_row!(t, "SELECT block_hash FROM blocks ORDER BY block_height DESC, discovered_at ASC LIMIT 1"; h: Hash; h).optional()?;
        execute!(t, "INSERT INTO blocks (block_hash, parent_hash, nonce) VALUES (x'deadface', ?, 0)", &parent_hash)?;

        while rv.len() < limit as usize {
            let all_tentative_txns = query_vec!(t, "SELECT transaction_hash, payer, signature FROM all_tentative_txns ORDER BY discovered_at ASC LIMIT ?", &(limit - (rv.len() as u16));
                                                h: Hash, p: PayerPublicKey, s: Signature; (h, p, s))?;
            if all_tentative_txns.is_empty() {
                break; // Found all tentative txns.
            }
            let mut progress = false;
            for (h, p, s) in all_tentative_txns.into_iter() {
                let mut sp = t.savepoint()?;
                execute!(sp, "INSERT INTO transaction_in_block (transaction_hash, block_hash, transaction_index) VALUES (?, x'deadface', ?)",
                         &h, &(rv.len() as u16))?;
                if query_row!(sp, "SELECT total_violations_count FROM block_consistency WHERE perspective_block = x'deadface'"; c: i64; c > 0)?
                {
                    sp.rollback()?
                } else {
                    sp.commit()?;
                    progress = true;
                    rv.push(BlockchainStorage::fill_transaction_in_out(&t, h, p, s)?);
                }
            }
            if !progress {
                // None of the remaining tentative transactions can be added to
                // the block (i.e. compatible with the block).
                break;
            }
        }
        Ok((rv, parent_hash))
    }

    pub fn get_ui_transaction_by_hash(self: &mut Self, h: &Hash) -> sql::Result<Option<Vec<(String, String)>>> {
        let t = self.conn.transaction()?; // TODO this ideally would not use a transaction, but a single statement.
        query_row!(t, "SELECT payer, signature, transaction_hash FROM transactions WHERE transaction_hash = ?", h;
                   p: PayerPublicKey, s: Signature, h:Hash;
                   BlockchainStorage::fill_transaction_in_out(&t, h, p, s)?
        ).optional()?
        .map_or(Ok(None), |tx| {
            let mut rv = Vec::new();
                rv.push(("Transaction Hash".to_owned(), h.display_hex()));
            rv.push(("Originating Wallet".to_owned(), Hash::sha256(&tx.payer.0).display_base58()));
            for (i, tx_output) in tx.outputs.into_iter().enumerate() {
                rv.push((format!("Output {} Amount", i), tx_output.amount.to_string()));
                rv.push((format!("Output {} Recipient", i), tx_output.recipient_hash.display_base58()));
            }
            if tx.inputs.is_empty() {
                rv.push(("Input".to_owned(), "None (this is a miner reward)".to_owned()));
            }
            for (i, tx_input) in tx.inputs.into_iter().enumerate() {
                rv.push((format!("Input {}", i), format!("{}.{}", tx_input.transaction_hash.display_hex(), tx_input.output_index)));
            }
            if let Some((cr, db)) = query_row!(t, "SELECT credited_amount, debited_amount FROM transaction_credit_debit WHERE transaction_hash = ?", h; cr: i64, db: i64; (cr, db)).optional()? {
                rv.push(("Credit Amount".to_owned(), cr.to_string()));
                rv.push(("Debit Amount".to_owned(), db.to_string()));
            }
            let conf =
                query_row!(t, "SELECT ifnull((SELECT longest_chain.confirmations FROM transaction_in_block JOIN longest_chain USING (block_hash) WHERE transaction_hash = ?), 0)", h; c: i64; c)?;
            rv.push(("Confirmations".to_owned(), conf.to_string()));
            Ok(Some(rv))
        })
    }

    pub fn prepare_mineable_block(self: &mut Self, miner_wallet: Option<&Wallet>) -> sql::Result<Block> {
        let miner_wallet = miner_wallet.unwrap_or(&self.default_wallet);
        let mut block = Block::new_mine_block(miner_wallet);
        let (mut new_tx, parent_hash) = self.get_mineable_tentative_transactions(None)?;
        block.transactions.append(&mut new_tx);
        block.parent_hash = parent_hash;
        Ok(block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_amount() {
        assert_eq!(format!("{}", Amount(0)), "0.00000000".to_owned());
        assert_eq!(format!("{}", Amount(1)), "0.00000001".to_owned());
        assert_eq!(format!("{}", Amount(100)), "0.00000100".to_owned());
        assert_eq!(format!("{}", Amount::COIN), "1.00000000".to_owned());
        assert_eq!(format!("{}", Amount::COIN * 10), "10.00000000".to_owned());
        assert_eq!(format!("{}", Amount::COIN * 1000), "1,000.00000000".to_owned());
        assert_eq!(format!("{}", Amount::COIN * 1234567), "1,234,567.00000000".to_owned());
        assert_eq!(format!("{}", Amount::MAX_MONEY), "100,000,000,000.00000000".to_owned());
    }

    #[test]
    fn can_create_wallet() {
        let w = Wallet::new();
        assert!(w.public_serialized.check_len());
    }

    #[test]
    fn can_create_raw_transaction() {
        let w = Wallet::new();
        w.create_raw_transaction(vec![], vec![]);
    }

    #[test]
    fn round_trips_to_disk() {
        let w = Wallet::new();
        assert!(w.save_to_disk().is_ok());
        let w2 = Wallet::load_from_disk().unwrap();
        assert_eq!(w, w2);
    }

    #[test]
    fn serialized_block_has_nonce_first() {
        let b =
            Block { nonce: 0x4142434445464748, transactions: vec![], parent_hash: None, block_hash: Hash::zeroes() };
        assert_eq!(&b.to_hash_challenge()[0..8], bincode::serialize(&b.nonce).unwrap().as_slice());
    }

    #[test]
    fn can_solve_hash_challenge() {
        let mut b = Block { nonce: 0, transactions: vec![], parent_hash: None, block_hash: Hash::zeroes() };
        assert!(b.solve_hash_challenge(16, None));
        eprintln!("Block with solved hash challenge: {:?}", b);
        assert_ne!(b.block_hash, Hash::zeroes());
        assert!(b.verify_hash_challenge(16));
    }

    #[test]
    fn can_create_bs() {
        BlockchainStorage::new(None, None);
        let path = std::path::Path::new("/tmp/storage.db");
        BlockchainStorage::new(Some(&path), None);
        assert!(path.exists());
    }

    #[test]
    fn can_recreate_db() {
        let path = std::path::Path::new("/tmp/storage.db");
        let mut bs = BlockchainStorage::new(Some(&path), None);
        // TODO add some stuff to the db and later check it's not there
        bs.recreate_db();
    }

    #[test]
    fn can_produce_empty_stats() {
        let bs = BlockchainStorage::new(None, None);
        assert_eq!(bs.produce_stats().unwrap(), BlockchainStats { pending_txn_count: 0, block_count: 0 });
    }

    #[test]
    fn can_create_trustworthy_wallet() {
        let mut bs = BlockchainStorage::new(None, None);
        bs.make_wallet().unwrap();
        assert_eq!(
            bs.conn
                .query_row("SELECT count(*) FROM trustworthy_wallets", sql::NO_PARAMS, |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn initial_default_wallet_zero_balance() {
        let mut bs = BlockchainStorage::new(None, None);
        let h = Hash::sha256(&bs.default_wallet.public_serialized.0);
        assert_eq!(bs.find_wallet_balance(&h, 0).unwrap(), 0);
        assert_eq!(BlockchainStorage::find_available_spend(&bs.conn.transaction().unwrap(), &h).unwrap().count(), 0);
    }

    #[test]
    fn initial_no_tentative_txns() {
        let mut bs = BlockchainStorage::new(None, None);
        assert!(bs.get_all_tentative_transactions().unwrap().is_empty());
        assert!(bs.get_mineable_tentative_transactions(None).unwrap().0.is_empty());
    }

    #[test]
    fn can_mine_genesis_block() {
        let w = Wallet::new();
        let mut bs = BlockchainStorage::new(None, Some(&w));
        let mut block = bs.prepare_mineable_block(None).unwrap();
        assert!(block.solve_hash_challenge(MINIMUM_DIFFICULTY_LEVEL, None));
        bs.receive_block(&block).unwrap();
        assert_eq!(bs.get_block_by_hash(&block.block_hash).unwrap(), Some(block));
        assert_eq!(bs.find_wallet_balance(w.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0);
    }

    #[test]
    fn can_receive_genesis_block() {
        let w1 = Wallet::new();
        let mut bs1 = BlockchainStorage::new(None, Some(&w1));
        let w2 = Wallet::new();
        let mut bs2 = BlockchainStorage::new(None, Some(&w2));
        {
            let mut block = bs1.prepare_mineable_block(None).unwrap();
            assert!(block.solve_hash_challenge(MINIMUM_DIFFICULTY_LEVEL, None));
            bs1.receive_block(&block).unwrap();
            bs2.receive_block(&block).unwrap();
        }
        assert_eq!(bs1.find_wallet_balance(w1.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0);
        assert_eq!(bs2.find_wallet_balance(w1.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0);
    }

    #[test]
    fn can_send_money() {
        let w1 = Wallet::new();
        let mut bs1 = BlockchainStorage::new(None, Some(&w1));
        let w2 = Wallet::new();
        let mut bs2 = BlockchainStorage::new(None, Some(&w2));
        {
            let mut block = bs1.prepare_mineable_block(None).unwrap();
            assert!(block.solve_hash_challenge(MINIMUM_DIFFICULTY_LEVEL, None));
            bs1.receive_block(&block).unwrap();
            bs2.receive_block(&block).unwrap();
        }

        // Create the transactions
        let tx = bs1.create_simple_transaction(None, Amount(10000), w2.public_key_hash()).unwrap();

        // Now tentative transactions should be non-empty
        assert_eq!(bs1.get_all_tentative_transactions().unwrap().len(), 1);

        // Available balance has been reduced in bs1, but not bs2
        assert_eq!(bs1.find_wallet_balance(w1.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0 - 10000);
        assert_eq!(bs2.find_wallet_balance(w1.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0);

        // bs2 can receive this transaction
        bs2.receive_tentative_transaction(&tx).unwrap();

        // From bs2's perspective, w1 has no more money left because the reward has been spent, but the change is unconfirmed.
        assert_eq!(bs2.find_wallet_balance(w1.public_key_hash(), 0).unwrap(), 0);

        // Both see one tentative tx
        assert_eq!(bs1.get_all_tentative_transactions().unwrap().len(), 1);
        assert_eq!(bs2.get_all_tentative_transactions().unwrap().len(), 1);

        // bs2 can then mine it
        {
            let mut block = bs2.prepare_mineable_block(None).unwrap();
            assert!(block.solve_hash_challenge(MINIMUM_DIFFICULTY_LEVEL, None));
            bs1.receive_block(&block).unwrap();
            bs2.receive_block(&block).unwrap();
        }

        // Both have a consistent view of the resulting balances
        assert_eq!(bs1.find_wallet_balance(w1.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0 - 10000);
        assert_eq!(bs2.find_wallet_balance(w1.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0 - 10000);
        assert_eq!(bs1.find_wallet_balance(w2.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0 + 10000);
        assert_eq!(bs2.find_wallet_balance(w2.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0 + 10000);
    }

    #[test]
    fn can_accept_orphaned_tentative_txns() {
        let w1 = Wallet::new();
        let mut bs1 = BlockchainStorage::new(None, Some(&w1));
        let w2 = Wallet::new();
        let mut bs2 = BlockchainStorage::new(None, Some(&w2));
        {
            let mut block = bs1.prepare_mineable_block(None).unwrap();
            assert!(block.solve_hash_challenge(MINIMUM_DIFFICULTY_LEVEL, None));
            bs1.receive_block(&block).unwrap();
            bs2.receive_block(&block).unwrap();
        }

        // Create two transactions, the latter is dependent on the UTXO of the first.
        let tx1 = bs1.create_simple_transaction(None, Amount(12345), w2.public_key_hash()).unwrap();
        let tx2 = bs1.create_simple_transaction(None, Amount(23456), w2.public_key_hash()).unwrap();

        assert_eq!(tx2.inputs.len(), 1);
        assert_eq!(tx2.inputs[0].transaction_hash, *tx1.transaction_hash());

        // bs2 can receive them out of order
        bs2.receive_tentative_transaction(&tx2).unwrap();
        bs2.receive_tentative_transaction(&tx1).unwrap();

        // Both have a consistent view, if bs2 trusts unconfirmed transactions from bs1
        bs2.make_wallet_trustworthy(&w1.public_hash).unwrap();
        assert_eq!(bs1.find_wallet_balance(w1.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0 - 12345 - 23456);
        assert_eq!(bs2.find_wallet_balance(w1.public_key_hash(), 0).unwrap(), Amount::BLOCK_REWARD.0 - 12345 - 23456);
        assert_eq!(bs1.find_wallet_balance(w2.public_key_hash(), 0).unwrap(), 12345 + 23456);
        assert_eq!(bs2.find_wallet_balance(w2.public_key_hash(), 0).unwrap(), 12345 + 23456);
    }

    #[test]
    fn can_accept_conflicting_tentative_txns() {
        let w1 = Wallet::new();
        let mut bs1a = BlockchainStorage::new(None, Some(&w1));
        let mut bs1b = BlockchainStorage::new(None, Some(&w1));
        let w2 = Wallet::new();
        let mut bs2 = BlockchainStorage::new(None, Some(&w2));
        let w3 = Wallet::new();
        {
            let mut block = bs1a.prepare_mineable_block(None).unwrap();
            assert!(block.solve_hash_challenge(MINIMUM_DIFFICULTY_LEVEL, None));
            bs1a.receive_block(&block).unwrap();
            bs1b.receive_block(&block).unwrap();
            bs2.receive_block(&block).unwrap();
        }

        // Now w1 attempts to spend the money twice, creating a conflict.
        let tx1 = bs1a.create_simple_transaction(None, Amount(12345), w2.public_key_hash()).unwrap();
        let tx2 = bs1b.create_simple_transaction(None, Amount(23456), w3.public_key_hash()).unwrap();

        // All of them can accept the tentative transactions successfully.
        bs1b.receive_tentative_transaction(&tx1).unwrap();
        bs1a.receive_tentative_transaction(&tx2).unwrap();
        bs2.receive_tentative_transaction(&tx1).unwrap();
        bs2.receive_tentative_transaction(&tx2).unwrap();

        // They have different views of w1's balance, none of which are totally
        // correct: bs1a and bs1b trust their own wallets, so they counted both
        // transactions as correct, whereas bs2 does not trust it, and so it
        // counted neither, and without the change UTXO.
        assert_eq!(
            bs1a.find_wallet_balance(w1.public_key_hash(), 0).unwrap(),
            Amount::BLOCK_REWARD.0 * 2 - 12345 - 23456
        );
        assert_eq!(
            bs1b.find_wallet_balance(w1.public_key_hash(), 0).unwrap(),
            Amount::BLOCK_REWARD.0 * 2 - 12345 - 23456
        );
        assert_eq!(bs2.find_wallet_balance(w1.public_key_hash(), 0).unwrap(), 0);
    }
}
