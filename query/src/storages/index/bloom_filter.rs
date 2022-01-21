// Copyright 2021 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use bincode;
use common_datablocks::DataBlock;
use common_datavalues::prelude::DataColumn;
use common_datavalues::seahash::SeaHasher;
use common_datavalues::DFHasher;
use common_datavalues::DataField;
use common_datavalues::DataSchema;
use common_datavalues::DataType;
use common_datavalues::DataValue;
use common_exception::ErrorCode;
use common_exception::Result;
use common_planners::Expression;
use common_tracing::tracing;

use crate::storages::index::IndexSchemaVersion;

/// BloomFilterExprEvalResult represents the evaluation result of an expression by bloom filter.
///
/// For example, expression of 'age = 12' should return false is the bloom filter are sure
/// of the nonexistent of value '12' in column 'age'. Otherwise should return 'Unknown'.
///
/// If the column is not applicable for bloom filter, like DataType::struct, NotApplicable is used.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BloomFilterExprEvalResult {
    False,
    Unknown,
    NotApplicable,
}

/// BloomFilterIndex represents multiple  bloom filters (per column) in data block.
///
/// By default we create bloom filter per column for a parquet data file. For columns whose data_type
/// are not applicable for a bloom filter, we skip the creation.
/// That is to say, it is legal to have a BloomFilterBlock with zero columns.
///
/// For example, for the data block as follows:
///```
///         +---name--+--age--+
///         | "Alice" |  20   |
///         | "Bob"   |  30   |
///         +---------+-------+
/// ```
/// We will create bloom filter files
///```
///         +---Bloom(name)--+--Bloom(age)--+
///         |  123456789abcd |  ac2345bcd   |
///         +----------------+--------------+
/// ```
pub struct BloomFilterIndexer {
    pub inner: DataBlock,
}

const BLOOM_FILTER_MAX_NUM_BITS: usize = 2048;
const BLOOM_FILTER_DEFAULT_FALSE_POSITIVE_RATE: f64 = 0.01;

impl BloomFilterIndexer {
    /// For every applicable column, we will create a bloom filter.
    /// The bloom filter will be stored with field name 'Bloom(column_name)'
    pub fn to_bloom_column_name(column_name: &str) -> String {
        format!("Bloom({})", column_name)
    }

    #[inline(always)]
    fn create_seeds() -> [u64; 4] {
        let seed0: u64 = rand::random();
        let seed1: u64 = rand::random();
        let seed2: u64 = rand::random();
        let seed3: u64 = rand::random();
        [seed0, seed1, seed2, seed3]
    }

    /// Create a bloom filter block from input data blocks.
    ///
    /// All input blocks should be belong to a Parquet file, e.g. the block array represents the parquet file in memory.
    pub fn from_data(blocks: &[DataBlock]) -> Result<Self> {
        if blocks.is_empty() {
            return Err(ErrorCode::BadArguments("data blocks is empty"));
        }

        let total_num_rows = blocks.iter().map(|block| block.num_rows() as u64).sum();

        let mut bloom_columns = vec![];
        let mut bloom_fields = vec![];

        let fields = blocks[0].schema().fields();
        for (i, field) in fields.iter().enumerate() {
            if BloomFilter::is_supported_type(field.data_type()) {
                // create field
                let bloom_column_name = Self::to_bloom_column_name(field.name());
                let bloom_field = DataField::new(&bloom_column_name, DataType::String, false);
                bloom_fields.push(bloom_field);

                // create bloom filter per column
                let seeds = Self::create_seeds();
                let mut bloom_filter = BloomFilter::with_rate_and_max_bits(
                    total_num_rows,
                    BLOOM_FILTER_DEFAULT_FALSE_POSITIVE_RATE,
                    BLOOM_FILTER_MAX_NUM_BITS,
                    seeds,
                );

                // ingest the same column data from all blocks
                for block in blocks.iter() {
                    let col = block.column(i);
                    bloom_filter.add(col)?;
                }

                // create bloom filter column
                let serialized_bytes = bloom_filter.to_vec()?;
                let bloom_column =
                    DataColumn::Constant(DataValue::String(Some(serialized_bytes)), 1);
                bloom_columns.push(bloom_column);
            }
        }

        let schema = Arc::new(DataSchema::new(bloom_fields));
        let block = DataBlock::create(schema, bloom_columns);
        Ok(Self { inner: block })
    }

    fn find(&self, column_name: &str, target: DataValue) -> Result<BloomFilterExprEvalResult> {
        let bloom_column = Self::to_bloom_column_name(column_name);
        if !self.inner.schema().has_field(&bloom_column)
            || !BloomFilter::is_supported_value(&target)
        {
            // The column doesn't have bloom filter bitmap
            return Ok(BloomFilterExprEvalResult::NotApplicable);
        }

        let val = self.inner.first(&bloom_column)?;
        let bloom_bytes = val.as_string()?;
        let bloom_filter = BloomFilter::from_vec(bloom_bytes.as_ref())?;
        if bloom_filter.find(target)? {
            Ok(BloomFilterExprEvalResult::Unknown)
        } else {
            Ok(BloomFilterExprEvalResult::False)
        }
    }

    /// Returns false when the expression must be false, otherwise true.
    /// The 'true' doesn't really mean the expression is true, but 'maybe true'.
    /// That is to say, you still need the load all data and run the execution.
    #[allow(dead_code)]
    pub fn maybe_true(&self, expr: &Expression) -> Result<bool> {
        Ok(self.eval(expr)? != BloomFilterExprEvalResult::False)
    }

    /// Apply the predicate expression, return the result.
    /// If we are sure of skipping the scan, return false, e.g. the expression must be false.
    /// This happens when the data doesn't show up in bloom filter.
    ///
    /// Otherwise return either Unknown or NotApplicable.
    pub fn eval(&self, expr: &Expression) -> Result<BloomFilterExprEvalResult> {
        //TODO: support multiple columns and other ops like 'in' ...
        match expr {
            Expression::BinaryExpression { left, op, right } => match op.to_lowercase().as_str() {
                "=" => self.eval_equivalent_expression(left, right),
                "and" => self.eval_logical_and(left, right),
                "or" => self.eval_logical_or(left, right),
                _ => Ok(BloomFilterExprEvalResult::NotApplicable),
            },
            _ => Ok(BloomFilterExprEvalResult::NotApplicable),
        }
    }

    // Evaluate the equivalent expression like "name='Alice'"
    fn eval_equivalent_expression(
        &self,
        left: &Expression,
        right: &Expression,
    ) -> Result<BloomFilterExprEvalResult> {
        // For now only support single column like "name = 'Alice'"
        match (left, right) {
            // match the expression of 'column_name = literal constant'
            (Expression::Column(column), Expression::Literal { value, .. })
            | (Expression::Literal { value, .. }, Expression::Column(column)) => {
                self.find(column, value.clone())
            }
            _ => Ok(BloomFilterExprEvalResult::NotApplicable),
        }
    }

    // Evaluate the logical and expression
    fn eval_logical_and(
        &self,
        left: &Expression,
        right: &Expression,
    ) -> Result<BloomFilterExprEvalResult> {
        let left_result = self.eval(left)?;
        if left_result == BloomFilterExprEvalResult::False {
            return Ok(BloomFilterExprEvalResult::False);
        }

        let right_result = self.eval(right)?;
        if right_result == BloomFilterExprEvalResult::False {
            return Ok(BloomFilterExprEvalResult::False);
        }

        if left_result == BloomFilterExprEvalResult::NotApplicable
            || right_result == BloomFilterExprEvalResult::NotApplicable
        {
            Ok(BloomFilterExprEvalResult::NotApplicable)
        } else {
            Ok(BloomFilterExprEvalResult::Unknown)
        }
    }

    // Evaluate the logical or expression
    fn eval_logical_or(
        &self,
        left: &Expression,
        right: &Expression,
    ) -> Result<BloomFilterExprEvalResult> {
        let left_result = self.eval(left)?;
        let right_result = self.eval(right)?;
        match (&left_result, &right_result) {
            (&BloomFilterExprEvalResult::False, &BloomFilterExprEvalResult::False) => {
                Ok(BloomFilterExprEvalResult::False)
            }
            (&BloomFilterExprEvalResult::False, _) => Ok(right_result),
            (_, &BloomFilterExprEvalResult::False) => Ok(left_result),
            (&BloomFilterExprEvalResult::Unknown, &BloomFilterExprEvalResult::Unknown) => {
                Ok(BloomFilterExprEvalResult::Unknown)
            }
            _ => Ok(BloomFilterExprEvalResult::NotApplicable),
        }
    }
}

/// A bloom filter implementation for data column and values.
///
/// Most ideas/implementations are ported from Clickhouse.
/// https://github.com/ClickHouse/ClickHouse/blob/1bf375e2b761db5b99b0f403b90c412a530f4d5c/src/Interpreters/BloomFilter.cpp
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct BloomFilter {
    // Container for bitmap
    container: Vec<u64>,

    // The number of bits of the bitmap
    num_bits: usize,

    // The number of hashes for bloom filter. We use double hashing and mix the result
    // to achieve k hashes. The value doesn't really mean the number of hashing we actually compute.
    // For more details, see this paper: http://www.eecs.harvard.edu/~michaelm/postscripts/rsa2008.pdf
    num_hashes: usize,

    version: IndexSchemaVersion,

    // The seeding for hash function, for now we use Seahash lib, which need 4 seeds.
    seeds: [u64; 4],
}

impl BloomFilter {
    /// Create a bloom filter instance with estimated number of items and expected false positive rate.
    pub fn with_rate(num_items: u64, false_positive_rate: f64, seeds: [u64; 4]) -> Self {
        let num_bits = Self::optimal_num_bits(num_items, false_positive_rate);
        let num_hashes = Self::optimal_num_hashes(num_items, num_bits as u64);

        Self::with_size(num_bits, num_hashes, seeds)
    }

    /// Create a bloom filter instance with estimated number of items, expected false positive rate,
    /// and maximum number of bits.
    pub fn with_rate_and_max_bits(
        num_items: u64,
        false_positive_rate: f64,
        max_num_bits: usize,
        seeds: [u64; 4],
    ) -> Self {
        let mut num_bits = Self::optimal_num_bits(num_items, false_positive_rate);
        let num_hashes = Self::optimal_num_hashes(num_items, num_bits as u64);

        num_bits = std::cmp::min(num_bits, max_num_bits);

        Self::with_size(num_bits, num_hashes, seeds)
    }

    /// Create a bloom filter instance with specified bitmap length and number of hashes
    pub fn with_size(num_bits: usize, num_hashes: usize, seeds: [u64; 4]) -> Self {
        // calculate the number of u64 we need
        let num_u64 = (num_bits + 63) / 64;

        Self {
            container: vec![0; num_u64],
            seeds,
            num_hashes,
            num_bits,
            version: IndexSchemaVersion::V1,
        }
    }

    /// Calculate the number of bits for the bloom filter from the formula:
    /// m  = - n * ln(p) / (ln(2)^2)
    ///
    /// n: number of items
    ///
    /// p: false positive rate
    pub fn optimal_num_bits(num_items: u64, false_positive_rate: f64) -> usize {
        let power_of_ln2 = core::f32::consts::LN_2 as f64 * core::f32::consts::LN_2 as f64;
        let m = -(num_items as f64 * false_positive_rate.ln()) / power_of_ln2;
        let num_bits = m.ceil() as usize;
        tracing::info!("Bloom filter calculate optimal bits, num_bits: {}, num_items: {}, false_positive_rate: {}", num_bits, num_items, false_positive_rate);
        num_bits
    }

    /// Calculate the number of hashes for the bloom filter from the formula:
    /// k = m/n * ln(2)
    ///
    /// m: number of bits
    ///
    /// n: number of items
    pub fn optimal_num_hashes(num_items: u64, num_bits: u64) -> usize {
        let k = num_bits as f64 / num_items as f64 * core::f32::consts::LN_2 as f64;
        let num_hashes = std::cmp::max(2, k.ceil() as usize); // at least two hashes
        tracing::info!(
            "Bloom filter calculate optimal hashes, num_hashes: {}",
            num_hashes
        );
        num_hashes
    }

    /// Returns the number of bits of the bloom filter.
    pub fn num_bits(&self) -> usize {
        self.num_bits
    }

    /// Returns the number of hashes of the bloom filter.
    pub fn num_hashes(&self) -> usize {
        self.num_hashes
    }

    /// Returns whether the data type is supported by bloom filter.
    ///
    /// The supported types are most same as Databricks:
    /// https://docs.microsoft.com/en-us/azure/databricks/delta/optimizations/bloom-filters
    ///
    /// "Bloom filters support columns with the following (input) data types: byte, short, int,
    /// long, float, double, date, timestamp, and string."
    ///
    /// Nulls are not added to the Bloom
    /// filter, so any null related filter requires reading the data file. "
    pub fn is_supported_type(data_type: &DataType) -> bool {
        matches!(
            data_type,
            DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::Float32
                | DataType::Float64
                | DataType::Date16
                | DataType::Date32
                | DataType::DateTime32(_)
                | DataType::DateTime64(_, _)
                | DataType::Interval(_)
                | DataType::String
        )
    }

    /// Return whether the data value is supported by bloom filter.
    /// Nulls are not supported.
    pub fn is_supported_value(value: &DataValue) -> bool {
        Self::is_supported_type(&value.data_type()) && !value.is_null()
    }

    // Create hasher for bloom. Use seahash for now, may change to city hash.
    #[inline(always)]
    fn create_hasher(&self) -> DFHasher {
        let hasher =
            SeaHasher::with_seeds(self.seeds[0], self.seeds[1], self.seeds[2], self.seeds[3]);
        DFHasher::SeaHasher64(hasher, self.seeds)
    }

    #[inline(always)]
    // Set bits for bloom filter, ported from Clickhouse.
    // https://github.com/ClickHouse/ClickHouse/blob/1bf375e2b761db5b99b0f403b90c412a530f4d5c/src/Interpreters/BloomFilter.cpp#L67
    fn set_bits(&mut self, hash1: &u64, hash2: &u64) {
        let h1 = std::num::Wrapping(*hash1);
        let h2 = std::num::Wrapping(*hash2);

        for i in 0..self.num_hashes {
            let index = std::num::Wrapping(i as u64);
            let res = (h1 + index * h2 + index * index).0;
            let bit_pos = (res % self.num_bits as u64) as usize;
            self.container[bit_pos / 64] |= 1u64 << (bit_pos % 64);
        }
    }

    /// Add the column data into bloom filter, Nulls are skipped and not added.
    ///
    /// The design of skipping Nulls are arguably correct. For now we do the same as databricks.
    /// See the design of databricks https://docs.databricks.com/delta/optimizations/bloom-filters.html
    pub fn add(&mut self, column: &DataColumn) -> Result<()> {
        if !Self::is_supported_type(&column.data_type()) {
            return Err(ErrorCode::BadArguments(format!(
                "Unsupported data type: {} ",
                column.data_type()
            )));
        }

        let series = column.to_minimal_array()?;

        let hasher1 = self.create_hasher();
        let hash1_arr = series.vec_hash(hasher1)?;

        let hasher2 = self.create_hasher();
        let hash2_arr = series.vec_hash(hasher2)?;

        let validity = series.validity();
        let no_null = validity == None || series.null_count() == 0;
        let all_null = series.len() == series.null_count();

        if all_null {
            return Ok(());
        }

        if no_null {
            hash1_arr
                .into_no_null_iter()
                .zip(hash2_arr.into_no_null_iter())
                .for_each(|(h1, h2)| {
                    self.set_bits(h1, h2);
                });
        } else {
            let bitmap = validity.unwrap();
            bitmap
                .into_iter()
                .zip(hash1_arr.into_no_null_iter())
                .zip(hash2_arr.into_no_null_iter())
                .for_each(|((valid, h1), h2)| {
                    if valid {
                        self.set_bits(h1, h2);
                    }
                });
        }
        Ok(())
    }

    /// Check the existence of the data. The data value should not be Null.
    /// Use BloomFilter::is_supported_value to check before using this method.
    ///
    ///
    /// Notice: false positive may exist, e.g. return true doesn't guarantee the value exists.
    /// But returning false guarantees that it never ever showed up.
    ///
    /// Example:
    /// ```
    ///     let not_exist = BloomFilter::is_supported_value(data_value) && !bloom.find(data_value)?;
    ///
    /// ```
    pub fn find(&self, val: DataValue) -> Result<bool> {
        if !Self::is_supported_value(&val) {
            return Err(ErrorCode::BadArguments(format!(
                "Unsupported data value: {} ",
                val
            )));
        }

        let col = DataColumn::Constant(val, 1);
        let series = col.to_minimal_array()?;

        let hasher1 = self.create_hasher();
        let hash1_arr = series.vec_hash(hasher1)?;
        let hash1 = hash1_arr.inner().value(0);

        let hasher2 = self.create_hasher();
        let hash2_arr = series.vec_hash(hasher2)?;
        let hash2 = hash2_arr.inner().value(0);

        let h1 = std::num::Wrapping(hash1);
        let h2 = std::num::Wrapping(hash2);
        for i in 0..self.num_hashes {
            let index = std::num::Wrapping(i as u64);
            let res = (h1 + index * h2 + index * index).0;
            let bit_pos = (res % self.num_bits as u64) as usize;

            // If any bit is not 1 in bloom filter, it means the data never ever showed up before.
            let flag = self.container[bit_pos / 64] & (1 << (bit_pos % 64));
            if flag == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Serialize the bloom filter to byte vector.
    pub fn to_vec(&self) -> Result<Vec<u8>> {
        match bincode::serialize(self) {
            Ok(v) => Ok(v),
            Err(e) => Err(ErrorCode::SerdeError(format!(
                "bincode serialization error: {} ",
                e
            ))),
        }
    }

    /// Deserialize from a byte slice and return a bloom filter.
    pub fn from_vec(bytes: &[u8]) -> Result<Self> {
        match bincode::deserialize::<BloomFilter>(bytes) {
            Ok(bloom_filter) => Ok(bloom_filter),
            Err(e) => Err(ErrorCode::SerdeError(format!(
                "bincode deserialization error: {} ",
                e
            ))),
        }
    }
}
