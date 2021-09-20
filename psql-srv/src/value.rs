use arccstr::ArcCStr;
use chrono::NaiveDateTime;
use rust_decimal::Decimal;

/// A PostgreSQL data value that can be received from, or sent to, a PostgreSQL frontend.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Char(ArcCStr),
    Varchar(ArcCStr),
    Int(i32),
    Bigint(i64),
    Smallint(i16),
    Double(f64),
    Float(f32),
    Numeric(Decimal),
    Text(ArcCStr),
    Timestamp(NaiveDateTime),
    ByteArray(Vec<u8>),
}
