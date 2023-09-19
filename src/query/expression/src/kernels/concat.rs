// Copyright 2021 Datafuse Labs
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

use common_arrow::arrow::buffer::Buffer;
use common_exception::ErrorCode;
use common_exception::Result;
use itertools::Itertools;

use crate::types::array::ArrayColumnBuilder;
use crate::types::decimal::DecimalColumn;
use crate::types::map::KvColumnBuilder;
use crate::types::nullable::NullableColumn;
use crate::types::number::NumberColumn;
use crate::types::string::StringColumn;
use crate::types::AnyType;
use crate::types::ArgType;
use crate::types::ArrayType;
use crate::types::BitmapType;
use crate::types::BooleanType;
use crate::types::DateType;
use crate::types::EmptyArrayType;
use crate::types::EmptyMapType;
use crate::types::MapType;
use crate::types::NullType;
use crate::types::NullableType;
use crate::types::NumberType;
use crate::types::StringType;
use crate::types::TimestampType;
use crate::types::ValueType;
use crate::types::VariantType;
use crate::with_decimal_type;
use crate::with_number_mapped_type;
use crate::BlockEntry;
use crate::Column;
use crate::ColumnBuilder;
use crate::DataBlock;
use crate::Value;

impl DataBlock {
    pub fn concat(blocks: &[DataBlock]) -> Result<DataBlock> {
        if blocks.is_empty() {
            return Err(ErrorCode::EmptyData("Can't concat empty blocks"));
        }

        if blocks.len() == 1 {
            return Ok(blocks[0].clone());
        }

        let concat_columns = (0..blocks[0].num_columns())
            .map(|i| {
                debug_assert!(
                    blocks
                        .iter()
                        .map(|block| &block.get_by_offset(i).data_type)
                        .all_equal()
                );

                let columns = blocks
                    .iter()
                    .map(|block| {
                        let entry = &block.get_by_offset(i);
                        match &entry.value {
                            Value::Scalar(s) => ColumnBuilder::repeat(
                                &s.as_ref(),
                                block.num_rows(),
                                &entry.data_type,
                            )
                            .build(),
                            Value::Column(c) => c.clone(),
                        }
                    })
                    .collect::<Vec<_>>();

                BlockEntry::new(
                    blocks[0].get_by_offset(i).data_type.clone(),
                    Value::Column(Column::concat(&columns)),
                )
            })
            .collect();

        let num_rows = blocks.iter().map(|c| c.num_rows()).sum();

        Ok(DataBlock::new(concat_columns, num_rows))
    }
}

impl Column {
    pub fn concat(columns: &[Column]) -> Column {
        if columns.len() == 1 {
            return columns[0].clone();
        }
        let capacity = columns.iter().map(|c| c.len()).sum();

        match &columns[0] {
            Column::Null { .. } => Self::concat_arg_types::<NullType>(columns),
            Column::EmptyArray { .. } => Self::concat_arg_types::<EmptyArrayType>(columns),
            Column::EmptyMap { .. } => Self::concat_arg_types::<EmptyMapType>(columns),
            Column::Number(col) => with_number_mapped_type!(|NUM_TYPE| match col {
                NumberColumn::NUM_TYPE(_) => {
                    let columns = columns
                        .iter()
                        .map(|col| <NumberType<NUM_TYPE>>::try_downcast_column(col).unwrap())
                        .collect_vec();
                    let builder = Self::concat_primitive_types(&columns, capacity);
                    <NumberType<NUM_TYPE>>::upcast_column(<NumberType<NUM_TYPE>>::column_from_vec(
                        builder,
                        &[],
                    ))
                }
            }),
            Column::Decimal(col) => with_decimal_type!(|DECIMAL_TYPE| match col {
                DecimalColumn::DECIMAL_TYPE(_, size) => {
                    let columns = columns
                        .iter()
                        .map(|col| match col {
                            Column::Decimal(DecimalColumn::DECIMAL_TYPE(col, _)) => col.clone(),
                            _ => unreachable!(),
                        })
                        .collect_vec();
                    let builder = Self::concat_primitive_types(&columns, capacity);
                    Column::Decimal(DecimalColumn::DECIMAL_TYPE(builder.into(), *size))
                }
            }),
            Column::Boolean(_) => Self::concat_arg_types::<BooleanType>(columns),
            Column::String(_) => {
                let columns = columns
                    .iter()
                    .map(|col| StringType::try_downcast_column(col).unwrap())
                    .collect_vec();
                StringType::upcast_column(Self::concat_string_types(&columns, capacity))
            }
            Column::Timestamp(_) => {
                let columns = columns
                    .iter()
                    .map(|col| TimestampType::try_downcast_column(col).unwrap())
                    .collect_vec();
                let builder = Self::concat_primitive_types(&columns, capacity);
                let ts = <NumberType<i64>>::upcast_column(<NumberType<i64>>::column_from_vec(
                    builder,
                    &[],
                ))
                .into_number()
                .unwrap()
                .into_int64()
                .unwrap();
                Column::Timestamp(ts)
            }
            Column::Date(_) => {
                let columns = columns
                    .iter()
                    .map(|col| DateType::try_downcast_column(col).unwrap())
                    .collect_vec();

                let builder = Self::concat_primitive_types(&columns, capacity);
                let d = <NumberType<i32>>::upcast_column(<NumberType<i32>>::column_from_vec(
                    builder,
                    &[],
                ))
                .into_number()
                .unwrap()
                .into_int32()
                .unwrap();
                Column::Date(d)
            }
            Column::Array(col) => {
                let mut offsets = Vec::with_capacity(capacity + 1);
                offsets.push(0);
                let builder = ColumnBuilder::with_capacity(&col.values.data_type(), capacity);
                let builder = ArrayColumnBuilder { builder, offsets };
                Self::concat_value_types::<ArrayType<AnyType>>(builder, columns)
            }
            Column::Map(col) => {
                let mut offsets = Vec::with_capacity(capacity + 1);
                offsets.push(0);
                let builder = ColumnBuilder::from_column(
                    ColumnBuilder::with_capacity(&col.values.data_type(), capacity).build(),
                );
                let (key_builder, val_builder) = match builder {
                    ColumnBuilder::Tuple(fields) => (fields[0].clone(), fields[1].clone()),
                    _ => unreachable!(),
                };
                let builder = KvColumnBuilder {
                    keys: key_builder,
                    values: val_builder,
                };
                let builder = ArrayColumnBuilder { builder, offsets };
                Self::concat_value_types::<MapType<AnyType, AnyType>>(builder, columns)
            }
            Column::Bitmap(_) => {
                let columns = columns
                    .iter()
                    .map(|col| BitmapType::try_downcast_column(col).unwrap())
                    .collect_vec();
                BitmapType::upcast_column(Self::concat_string_types(&columns, capacity))
            }
            Column::Nullable(_) => {
                let mut bitmaps = Vec::with_capacity(columns.len());
                let mut inners = Vec::with_capacity(columns.len());
                for c in columns {
                    let nullable_column = NullableType::<AnyType>::try_downcast_column(c).unwrap();
                    inners.push(nullable_column.column);
                    bitmaps.push(Column::Boolean(nullable_column.validity));
                }

                let column = Self::concat(&inners);
                let validity = Self::concat_arg_types::<BooleanType>(&bitmaps);
                let validity = BooleanType::try_downcast_column(&validity).unwrap();

                Column::Nullable(Box::new(NullableColumn { column, validity }))
            }
            Column::Tuple(fields) => {
                let fields = (0..fields.len())
                    .map(|idx| {
                        let cs: Vec<Column> = columns
                            .iter()
                            .map(|col| col.as_tuple().unwrap()[idx].clone())
                            .collect();
                        Self::concat(&cs)
                    })
                    .collect();
                Column::Tuple(fields)
            }
            Column::Variant(_) => {
                let columns = columns
                    .iter()
                    .map(|col| VariantType::try_downcast_column(col).unwrap())
                    .collect_vec();
                VariantType::upcast_column(Self::concat_string_types(&columns, capacity))
            }
        }
    }

    pub fn concat_primitive_types<T>(cols: &[Buffer<T>], num_rows: usize) -> Vec<T>
    where T: Copy {
        let mut builder: Vec<T> = Vec::with_capacity(num_rows);
        let ptr = builder.as_mut_ptr();
        let mut i = 0;
        for col in cols {
            for item in col.iter() {
                // # Safety
                // `i` must be less than `num_rows`.
                unsafe {
                    std::ptr::write(ptr.add(i), *item);
                }
                i += 1;
            }
        }
        // # Safety
        // The capacity of builder is `num_rows`.
        unsafe { builder.set_len(num_rows) };
        builder
    }

    pub fn concat_string_types<'a>(cols: &'a [StringColumn], num_rows: usize) -> StringColumn {
        let mut items: Vec<&[u8]> = Vec::with_capacity(num_rows);
        let mut offsets: Vec<u64> = Vec::with_capacity(num_rows + 1);
        offsets.push(0);
        let items_ptr = items.as_mut_ptr();
        let offsets_ptr = unsafe { offsets.as_mut_ptr().add(1) };

        let mut i = 0;
        let mut data_size = 0;
        for col in cols {
            for item in col.iter() {
                data_size += item.len() as u64;
                // # Safety
                // `i` must be less than the capacity of Vec.
                unsafe {
                    std::ptr::write(items_ptr.add(i), item);
                    std::ptr::write(offsets_ptr.add(i), data_size);
                    i += 1;
                }
            }
        }
        unsafe {
            items.set_len(num_rows);
            offsets.set_len(num_rows + 1);
        }

        let mut data: Vec<u8> = Vec::with_capacity(data_size as usize);
        let data_ptr = data.as_mut_ptr();
        let mut offset = 0;
        for item in items {
            let len = item.len();
            // # Safety
            // `offset` + `len` < `data_size`.
            unsafe {
                std::ptr::copy_nonoverlapping(item.as_ptr(), data_ptr.add(offset), len);
            }
            offset += len;
        }
        unsafe { data.set_len(offset) };
        StringColumn::new(data.into(), offsets.into())
    }

    fn concat_arg_types<T: ArgType>(columns: &[Column]) -> Column {
        let columns: Vec<T::Column> = columns
            .iter()
            .map(|c| T::try_downcast_column(c).unwrap())
            .collect();
        let iter = columns.iter().flat_map(|c| T::iter_column(c));
        let result = T::column_from_ref_iter(iter, &[]);
        T::upcast_column(result)
    }

    fn concat_value_types<T: ValueType>(
        mut builder: T::ColumnBuilder,
        columns: &[Column],
    ) -> Column {
        let columns: Vec<T::Column> = columns
            .iter()
            .map(|c| T::try_downcast_column(c).unwrap())
            .collect();

        for col in columns {
            T::append_column(&mut builder, &col);
        }
        T::upcast_column(T::build_column(builder))
    }
}
