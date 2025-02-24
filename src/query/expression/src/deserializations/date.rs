// Copyright 2022 Datafuse Labs.
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

use std::io::Cursor;

use chrono::Datelike;
use chrono::NaiveDate;
use common_exception::ErrorCode;
use common_exception::Result;
use common_io::cursor_ext::BufferReadDateTimeExt;
use common_io::prelude::BinaryRead;
use common_io::prelude::FormatSettings;

use crate::types::date::check_date;
use crate::Column;
use crate::Scalar;
use crate::TypeDeserializer;

pub struct DateDeserializer {
    pub buffer: Vec<u8>,
    pub builder: Vec<i32>,
}

impl DateDeserializer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: vec![],
            builder: Vec::with_capacity(capacity),
        }
    }
}

impl TypeDeserializer for DateDeserializer {
    fn memory_size(&self) -> usize {
        self.builder.len() * std::mem::size_of::<i32>()
    }

    fn len(&self) -> usize {
        self.builder.len()
    }

    fn de_binary(&mut self, reader: &mut &[u8], _format: &FormatSettings) -> Result<()> {
        let value: i32 = reader.read_scalar()?;
        self.builder.push(value);
        Ok(())
    }

    fn de_default(&mut self) {
        self.builder.push(i32::default());
    }

    fn de_fixed_binary_batch(
        &mut self,
        reader: &[u8],
        step: usize,
        rows: usize,
        _format: &FormatSettings,
    ) -> Result<()> {
        for row in 0..rows {
            let mut reader = &reader[step * row..];
            let value: i32 = reader.read_scalar()?;
            self.builder.push(value);
        }
        Ok(())
    }

    fn de_json(&mut self, value: &serde_json::Value, format: &FormatSettings) -> Result<()> {
        match value {
            serde_json::Value::String(v) => {
                let mut reader = Cursor::new(v.as_bytes());
                let date = reader.read_date_text(&format.timezone)?;
                let days = uniform_date(date);
                self.builder.push(days);
                Ok(())
            }
            _ => Err(ErrorCode::from("Incorrect string value")),
        }
    }

    fn append_data_value(&mut self, value: Scalar, _format: &FormatSettings) -> Result<()> {
        let v = value
            .as_date()
            .ok_or_else(|| "Unable to get date value".to_string())?;
        check_date(*v as i64).map_err(ErrorCode::from_string)?;
        self.builder.push(*v);
        Ok(())
    }

    fn pop_data_value(&mut self) -> Result<Scalar> {
        match self.builder.pop() {
            Some(v) => Ok(Scalar::Date(v)),
            None => Err(ErrorCode::from("Date column is empty when pop data value")),
        }
    }

    fn finish_to_column(&mut self) -> Column {
        self.builder.shrink_to_fit();
        Column::Date(std::mem::take(&mut self.builder).into())
    }
}

pub const EPOCH_DAYS_FROM_CE: i32 = 719_163;

#[inline]
pub fn uniform_date(date: NaiveDate) -> i32 {
    date.num_days_from_ce() - EPOCH_DAYS_FROM_CE
}
