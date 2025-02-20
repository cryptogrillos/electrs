use anyhow::Result;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::{Block, BlockHash, OutPoint, Txid};

use crate::{
    chain::Chain,
    daemon::Daemon,
    db::{DBStore, Row, WriteBatch},
    metrics::{Gauge, Histogram, Metrics},
    signals::ExitFlag,
    types::{HeaderRow, ScriptHash, ScriptHashRow, SpendingPrefixRow, TxidRow},
};

#[derive(Clone)]
struct Stats {
    update_duration: Histogram,
    update_size: Histogram,
    height: Gauge,
}

impl Stats {
    fn new(metrics: &Metrics) -> Self {
        Self {
            update_duration: metrics.histogram_vec(
                "index_update_duration",
                "Index update duration (in seconds)",
                "step",
            ),
            update_size: metrics.histogram_vec(
                "index_update_size",
                "Index update size (in bytes)",
                "step",
            ),
            height: metrics.gauge("index_height", "Latest indexed block height"),
        }
    }
}

struct IndexResult {
    header_row: HeaderRow,
    funding_rows: Vec<ScriptHashRow>,
    spending_rows: Vec<SpendingPrefixRow>,
    txid_rows: Vec<TxidRow>,
}

impl IndexResult {
    fn extend(&self, batch: &mut WriteBatch) {
        let funding_rows = self.funding_rows.iter().map(ScriptHashRow::to_db_row);
        batch.funding_rows.extend(funding_rows);

        let spending_rows = self.spending_rows.iter().map(SpendingPrefixRow::to_db_row);
        batch.spending_rows.extend(spending_rows);

        let txid_rows = self.txid_rows.iter().map(TxidRow::to_db_row);
        batch.txid_rows.extend(txid_rows);

        batch.header_rows.push(self.header_row.to_db_row());
        batch.tip_row = serialize(&self.header_row.header.block_hash()).into_boxed_slice();
    }
}

/// Confirmed transactions' address index
pub struct Index {
    store: DBStore,
    batch_size: usize,
    lookup_limit: Option<usize>,
    chain: Chain,
    stats: Stats,
}

impl Index {
    pub(crate) fn load(
        store: DBStore,
        mut chain: Chain,
        metrics: &Metrics,
        batch_size: usize,
        lookup_limit: Option<usize>,
    ) -> Result<Self> {
        if let Some(row) = store.get_tip() {
            let tip = deserialize(&row).expect("invalid tip");
            let headers = store
                .read_headers()
                .into_iter()
                .map(|row| HeaderRow::from_db_row(&row).header)
                .collect();
            chain.load(headers, tip);
        };

        let stats = Stats::new(metrics);
        stats.height.set(chain.height());

        Ok(Index {
            store,
            batch_size,
            lookup_limit,
            chain,
            stats,
        })
    }

    pub(crate) fn chain(&self) -> &Chain {
        &self.chain
    }

    pub(crate) fn limit_result<T>(&self, entries: impl Iterator<Item = T>) -> Result<Vec<T>> {
        let mut entries = entries.fuse();
        let result: Vec<T> = match self.lookup_limit {
            Some(lookup_limit) => entries.by_ref().take(lookup_limit).collect(),
            None => entries.by_ref().collect(),
        };
        if entries.next().is_some() {
            bail!(">{} index entries, query may take too long", result.len())
        }
        Ok(result)
    }

    pub(crate) fn filter_by_txid(&self, txid: Txid) -> impl Iterator<Item = BlockHash> + '_ {
        self.store
            .iter_txid(TxidRow::scan_prefix(txid))
            .map(|row| TxidRow::from_db_row(&row).height())
            .filter_map(move |height| self.chain.get_block_hash(height))
    }

    pub(crate) fn filter_by_funding(
        &self,
        scripthash: ScriptHash,
    ) -> impl Iterator<Item = BlockHash> + '_ {
        self.store
            .iter_funding(ScriptHashRow::scan_prefix(scripthash))
            .map(|row| ScriptHashRow::from_db_row(&row).height())
            .filter_map(move |height| self.chain.get_block_hash(height))
    }

    pub(crate) fn filter_by_spending(
        &self,
        outpoint: OutPoint,
    ) -> impl Iterator<Item = BlockHash> + '_ {
        self.store
            .iter_spending(SpendingPrefixRow::scan_prefix(outpoint))
            .map(|row| SpendingPrefixRow::from_db_row(&row).height())
            .filter_map(move |height| self.chain.get_block_hash(height))
    }

    fn observe_duration<T>(&self, label: &str, f: impl FnOnce() -> T) -> T {
        self.stats.update_duration.observe_duration(label, f)
    }

    fn observe_size(&self, label: &str, rows: &[Row]) {
        self.stats.update_size.observe(label, db_rows_size(rows));
    }

    fn report_stats(&self, batch: &WriteBatch) {
        self.observe_size("write_funding_rows", &batch.funding_rows);
        self.observe_size("write_spending_rows", &batch.spending_rows);
        self.observe_size("write_txid_rows", &batch.txid_rows);
        self.observe_size("write_header_rows", &batch.header_rows);
        debug!(
            "writing {} funding and {} spending rows from {} transactions, {} blocks",
            batch.funding_rows.len(),
            batch.spending_rows.len(),
            batch.txid_rows.len(),
            batch.header_rows.len()
        );
    }

    pub(crate) fn sync(&mut self, daemon: &Daemon, exit_flag: &ExitFlag) -> Result<()> {
        while !exit_flag.is_set() {
            let new_headers =
                self.observe_duration("headers", || daemon.get_new_headers(&self.chain))?;
            if new_headers.is_empty() {
                break;
            }
            info!(
                "indexing {} blocks: [{}..{}]",
                new_headers.len(),
                new_headers.first().unwrap().height(),
                new_headers.last().unwrap().height()
            );
            for chunk in new_headers.chunks(self.batch_size) {
                if exit_flag.is_set() {
                    bail!(
                        "indexing interrupted at height: {}",
                        chunk.first().unwrap().height()
                    )
                }
                let blockhashes: Vec<BlockHash> = chunk.iter().map(|h| h.hash()).collect();
                let mut heights = chunk.iter().map(|h| h.height());

                let mut batch = WriteBatch::default();
                daemon.for_blocks(blockhashes, |_blockhash, block| {
                    let height = heights.next().expect("unexpected block");
                    self.observe_duration("block", || {
                        index_single_block(block, height).extend(&mut batch)
                    });
                    self.stats.height.set(height);
                })?;
                let heights: Vec<_> = heights.collect();
                assert!(
                    heights.is_empty(),
                    "some blocks were not indexed: {:?}",
                    heights
                );
                batch.sort();
                self.report_stats(&batch);
                self.observe_duration("write", || self.store.write(batch));
            }
            self.chain.update(new_headers);
        }
        self.store.flush();
        Ok(())
    }
}

fn db_rows_size(rows: &[Row]) -> usize {
    rows.iter().map(|key| key.len()).sum()
}

fn index_single_block(block: Block, height: usize) -> IndexResult {
    let mut funding_rows = vec![];
    let mut spending_rows = vec![];
    let mut txid_rows = Vec::with_capacity(block.txdata.len());

    for tx in &block.txdata {
        txid_rows.push(TxidRow::new(tx.txid(), height));

        funding_rows.extend(
            tx.output
                .iter()
                .filter(|txo| !txo.script_pubkey.is_provably_unspendable())
                .map(|txo| {
                    let scripthash = ScriptHash::new(&txo.script_pubkey);
                    ScriptHashRow::new(scripthash, height)
                }),
        );

        if tx.is_coin_base() {
            continue; // coinbase doesn't have inputs
        }
        spending_rows.extend(
            tx.input
                .iter()
                .map(|txin| SpendingPrefixRow::new(txin.previous_output, height)),
        );
    }
    IndexResult {
        funding_rows,
        spending_rows,
        txid_rows,
        header_row: HeaderRow::new(block.header),
    }
}
