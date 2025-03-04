// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A permanent storage encoding for rows.
//!
//! See row.proto for details.

use bytes::BufMut;
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Timelike, Utc};
use dec::Decimal;
use ore::cast::CastFrom;
use persist_types::Codec;
use prost::Message;
use uuid::Uuid;

use crate::adt::array::ArrayDimension;
use crate::adt::interval::Interval;
use crate::adt::numeric::Numeric;
use crate::gen::row::proto_datum::DatumType;
use crate::gen::row::{
    ProtoArray, ProtoArrayDimension, ProtoDate, ProtoDatum, ProtoDatumOther, ProtoDict,
    ProtoDictElement, ProtoInterval, ProtoNumeric, ProtoRow, ProtoTime, ProtoTimestamp,
};
use crate::{Datum, Row};

impl Codec for Row {
    fn codec_name() -> String {
        "protobuf[Row]".into()
    }

    /// Encodes a row into the permanent storage format.
    ///
    /// This perfectly round-trips through [Row::decode]. It's guaranteed to be
    /// readable by future versions of Materialize through v(TODO: Figure out
    /// our policy).
    fn encode<B>(&self, buf: &mut B)
    where
        B: BufMut,
    {
        ProtoRow::from(self)
            .encode(buf)
            .expect("no required fields means no initialization errors");
    }

    /// Decodes a row from the permanent storage format.
    ///
    /// This perfectly round-trips through [Row::encode]. It can read rows
    /// encoded by historical versions of Materialize back to v(TODO: Figure out
    /// our policy).
    fn decode(buf: &[u8]) -> Result<Row, String> {
        let proto_row = ProtoRow::decode(buf).map_err(|err| err.to_string())?;
        Row::try_from(&proto_row)
    }
}

impl<'a> From<Datum<'a>> for ProtoDatum {
    fn from(x: Datum<'a>) -> Self {
        let datum_type = match x {
            Datum::False => DatumType::Other(ProtoDatumOther::False.into()),
            Datum::True => DatumType::Other(ProtoDatumOther::True.into()),
            Datum::Int16(x) => DatumType::Int16(x.into()),
            Datum::Int32(x) => DatumType::Int32(x),
            Datum::Int64(x) => DatumType::Int64(x),
            Datum::Float32(x) => DatumType::Float32(x.into_inner()),
            Datum::Float64(x) => DatumType::Float64(x.into_inner()),
            Datum::Date(x) => DatumType::Date(ProtoDate {
                year: x.year(),
                ordinal: x.ordinal(),
            }),
            Datum::Time(x) => DatumType::Time(ProtoTime {
                secs: x.num_seconds_from_midnight(),
                nanos: x.nanosecond(),
            }),
            Datum::Timestamp(x) => DatumType::Timestamp(ProtoTimestamp {
                year: x.date().year(),
                ordinal: x.date().ordinal(),
                secs: x.time().num_seconds_from_midnight(),
                nanos: x.time().nanosecond(),
                is_tz: false,
            }),
            Datum::TimestampTz(x) => {
                let date = x.date().naive_utc();
                DatumType::Timestamp(ProtoTimestamp {
                    year: date.year(),
                    ordinal: date.ordinal(),
                    secs: x.time().num_seconds_from_midnight(),
                    nanos: x.time().nanosecond(),
                    is_tz: true,
                })
            }
            Datum::Interval(x) => {
                let duration = x.duration.to_le_bytes();
                let (mut duration_lo, mut duration_hi) = ([0u8; 8], [0u8; 8]);
                duration_lo.copy_from_slice(&duration[..8]);
                duration_hi.copy_from_slice(&duration[8..]);
                DatumType::Interval(ProtoInterval {
                    months: x.months,
                    duration_lo: i64::from_le_bytes(duration_lo),
                    duration_hi: i64::from_le_bytes(duration_hi),
                })
            }
            Datum::Bytes(x) => DatumType::Bytes(x.to_vec()),
            Datum::String(x) => DatumType::String(x.to_owned()),
            Datum::Array(x) => DatumType::Array(ProtoArray {
                elements: Some(ProtoRow {
                    datums: x.elements().iter().map(|x| x.into()).collect(),
                }),
                dims: x
                    .dims()
                    .into_iter()
                    .map(|x| ProtoArrayDimension {
                        lower_bound: u64::cast_from(x.lower_bound),
                        length: u64::cast_from(x.length),
                    })
                    .collect(),
            }),
            Datum::List(x) => DatumType::List(ProtoRow {
                datums: x.iter().map(|x| x.into()).collect(),
            }),
            Datum::Map(x) => DatumType::Dict(ProtoDict {
                elements: x
                    .iter()
                    .map(|(k, v)| ProtoDictElement {
                        key: k.to_owned(),
                        val: Some(v.into()),
                    })
                    .collect(),
            }),
            Datum::Numeric(x) => {
                // TODO: Do we need this defensive clone?
                let mut x = x.0.clone();
                if let Some((bcd, scale)) = x.to_packed_bcd() {
                    DatumType::Numeric(ProtoNumeric { bcd, scale })
                } else if x.is_nan() {
                    DatumType::Other(ProtoDatumOther::NumericNaN.into())
                } else if x.is_infinite() {
                    if x.is_negative() {
                        DatumType::Other(ProtoDatumOther::NumericNegInf.into())
                    } else {
                        DatumType::Other(ProtoDatumOther::NumericPosInf.into())
                    }
                } else if x.is_special() {
                    panic!("internal error: unhandled special numeric value: {}", x);
                } else {
                    panic!(
                        "internal error: to_packed_bcd returned None for non-special value: {}",
                        x
                    )
                }
            }
            Datum::JsonNull => DatumType::Other(ProtoDatumOther::JsonNull.into()),
            Datum::Uuid(x) => DatumType::Uuid(x.as_bytes().to_vec()),
            Datum::Dummy => DatumType::Other(ProtoDatumOther::Dummy.into()),
            Datum::Null => DatumType::Other(ProtoDatumOther::Null.into()),
        };
        ProtoDatum {
            datum_type: Some(datum_type),
        }
    }
}

impl From<&Row> for ProtoRow {
    fn from(x: &Row) -> Self {
        let datums = x.iter().map(|x| x.into()).collect();
        ProtoRow { datums }
    }
}

impl Row {
    fn try_push_proto(&mut self, x: &ProtoDatum) -> Result<(), String> {
        match &x.datum_type {
            Some(DatumType::Other(o)) => match ProtoDatumOther::from_i32(*o) {
                Some(ProtoDatumOther::Unknown) => return Err("unknown datum type".into()),
                Some(ProtoDatumOther::Null) => self.push(Datum::Null),
                Some(ProtoDatumOther::False) => self.push(Datum::False),
                Some(ProtoDatumOther::True) => self.push(Datum::True),
                Some(ProtoDatumOther::JsonNull) => self.push(Datum::JsonNull),
                Some(ProtoDatumOther::Dummy) => self.push(Datum::Dummy),
                Some(ProtoDatumOther::NumericPosInf) => self.push(Datum::from(Numeric::infinity())),
                Some(ProtoDatumOther::NumericNegInf) => {
                    self.push(Datum::from(-Numeric::infinity()))
                }
                Some(ProtoDatumOther::NumericNaN) => self.push(Datum::from(Numeric::nan())),
                None => return Err(format!("unknown datum type: {}", o)),
            },
            Some(DatumType::Int16(x)) => {
                let x = i16::try_from(*x)
                    .map_err(|_| format!("int16 field stored with out of range value: {}", *x))?;
                self.push(Datum::Int16(x))
            }
            Some(DatumType::Int32(x)) => self.push(Datum::Int32(*x)),
            Some(DatumType::Int64(x)) => self.push(Datum::Int64(*x)),
            Some(DatumType::Float32(x)) => self.push(Datum::Float32((*x).into())),
            Some(DatumType::Float64(x)) => self.push(Datum::Float64((*x).into())),
            Some(DatumType::Bytes(x)) => self.push(Datum::Bytes(x)),
            Some(DatumType::String(x)) => self.push(Datum::String(x)),
            Some(DatumType::Uuid(x)) => {
                // Uuid internally has a [u8; 16] so we'll have to do at least
                // one copy, but there's currently an additional one when the
                // Vec is created. Perhaps the protobuf Bytes support will let
                // us fix one of them.
                let u = Uuid::from_slice(&x).map_err(|err| err.to_string())?;
                self.push(Datum::Uuid(u));
            }
            Some(DatumType::Date(x)) => {
                self.push(Datum::Date(NaiveDate::from_yo(x.year, x.ordinal)))
            }
            Some(DatumType::Time(x)) => self.push(Datum::Time(
                NaiveTime::from_num_seconds_from_midnight(x.secs, x.nanos),
            )),
            Some(DatumType::Timestamp(x)) => {
                let date = NaiveDate::from_yo(x.year, x.ordinal);
                let time = NaiveTime::from_num_seconds_from_midnight(x.secs, x.nanos);
                let datetime = date.and_time(time);
                if x.is_tz {
                    self.push(Datum::TimestampTz(DateTime::from_utc(datetime, Utc)));
                } else {
                    self.push(Datum::Timestamp(datetime));
                }
            }
            Some(DatumType::Interval(x)) => {
                let mut duration = [0u8; 16];
                duration[..8].copy_from_slice(&x.duration_lo.to_le_bytes());
                duration[8..].copy_from_slice(&x.duration_hi.to_le_bytes());
                let duration = i128::from_le_bytes(duration);
                self.push(Datum::Interval(Interval {
                    months: x.months,
                    duration,
                }))
            }
            Some(DatumType::List(x)) => self.push_list_with(|row| -> Result<(), String> {
                for d in x.datums.iter() {
                    row.try_push_proto(d)?;
                }
                Ok(())
            })?,
            Some(DatumType::Array(x)) => {
                let dims = x
                    .dims
                    .iter()
                    .map(|x| ArrayDimension {
                        lower_bound: usize::cast_from(x.lower_bound),
                        length: usize::cast_from(x.length),
                    })
                    .collect::<Vec<_>>();
                match x.elements.as_ref() {
                    None => self.push_array(&dims, vec![].iter()),
                    Some(elements) => {
                        // TODO: Could we avoid this Row alloc if we made a
                        // push_array_with?
                        let elements_row = Row::try_from(elements)?;
                        self.push_array(&dims, elements_row.iter())
                    }
                }
                .map_err(|err| err.to_string())?
            }
            Some(DatumType::Dict(x)) => self.push_dict_with(|row| -> Result<(), String> {
                for e in x.elements.iter() {
                    row.push(Datum::from(e.key.as_str()));
                    let val = e
                        .val
                        .as_ref()
                        .ok_or_else(|| format!("missing val for key: {}", e.key))?;
                    row.try_push_proto(val)?;
                }
                Ok(())
            })?,
            Some(DatumType::Numeric(x)) => {
                // Reminder that special values like NaN, PosInf, and NegInf are
                // represented as variants of ProtoDatumOther.
                let n = Decimal::from_packed_bcd(&x.bcd, x.scale).map_err(|err| err.to_string())?;
                self.push(Datum::from(n))
            }
            None => return Err("unknown datum type".into()),
        };
        Ok(())
    }
}

impl TryFrom<&ProtoRow> for Row {
    type Error = String;

    fn try_from(x: &ProtoRow) -> Result<Self, Self::Error> {
        // TODO: Try to pre-size this.
        let mut row = Row::default();
        for d in x.datums.iter() {
            row.try_push_proto(d)?;
        }
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
    use persist_types::Codec;
    use uuid::Uuid;

    use crate::adt::array::ArrayDimension;
    use crate::adt::interval::Interval;
    use crate::adt::numeric::Numeric;
    use crate::{Datum, Row};

    // TODO: datadriven golden tests for various interesting Datums and Rows to
    // catch any changes in the encoding.

    #[test]
    fn roundtrip() {
        let mut row = Row::pack(vec![
            Datum::False,
            Datum::True,
            Datum::Int16(1),
            Datum::Int32(2),
            Datum::Int64(3),
            Datum::Float32(4f32.into()),
            Datum::Float64(5f64.into()),
            Datum::Date(NaiveDate::from_ymd(6, 7, 8)),
            Datum::Time(NaiveTime::from_hms(9, 10, 11)),
            Datum::Timestamp(
                NaiveDate::from_ymd(12, 13 % 12, 14).and_time(NaiveTime::from_hms(15, 16, 17)),
            ),
            Datum::TimestampTz(DateTime::from_utc(
                NaiveDate::from_ymd(18, 19 % 12, 20).and_time(NaiveTime::from_hms(21, 22, 23)),
                Utc,
            )),
            Datum::Interval(Interval {
                months: 24,
                duration: 25,
            }),
            Datum::Bytes(&[26, 27]),
            Datum::String("28"),
            Datum::from(Numeric::from(29)),
            Datum::from(Numeric::infinity()),
            Datum::from(-Numeric::infinity()),
            Datum::from(Numeric::nan()),
            Datum::JsonNull,
            Datum::Uuid(Uuid::from_u128(30)),
            Datum::Dummy,
            Datum::Null,
        ]);
        row.push_array(
            &[ArrayDimension {
                lower_bound: 2,
                length: 2,
            }],
            vec![Datum::Int32(31), Datum::Int32(32)],
        )
        .expect("valid array");
        row.push_list_with(|row| {
            row.push(Datum::String("33"));
            row.push_list_with(|row| {
                row.push(Datum::String("34"));
                row.push(Datum::String("35"));
            });
            row.push(Datum::String("36"));
            row.push(Datum::String("37"));
        });
        row.push_dict_with(|row| {
            // Add a bunch of data to the hash to ensure we don't get a
            // HashMap's random iteration anywhere in the encode/decode path.
            let mut i = 38;
            for _ in 0..20 {
                row.push(Datum::String(&i.to_string()));
                row.push(Datum::Int32(i + 1));
                i += 2;
            }
        });

        let mut encoded = Vec::new();
        row.encode(&mut encoded);
        assert_eq!(Row::decode(&encoded), Ok(row));
    }
}
