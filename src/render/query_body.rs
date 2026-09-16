use sqlx::Arguments;
use sqlx::postgres::{PgArguments, Postgres};

/// How a condition compares its column with its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Lte,
    /// `>`
    Gt,
    /// `>=`
    Gte,
    /// `LIKE`
    Like,
    /// `= ANY(...)`, the value being a list.
    Any,
    /// `IS NULL`; there is no value.
    IsNull,
    /// `IS NOT NULL`; there is no value.
    IsNotNull,
}

impl Op {
    /// The operator a key suffix names: `lt` in `price__lt`. A bare key
    /// is `Eq`; `in` is `Any`.
    pub fn from_suffix(suffix: &str) -> Option<Self> {
        Some(match suffix {
            "eq" => Self::Eq,
            "ne" => Self::Ne,
            "lt" => Self::Lt,
            "lte" => Self::Lte,
            "gt" => Self::Gt,
            "gte" => Self::Gte,
            "like" => Self::Like,
            "in" => Self::Any,
            _ => return None,
        })
    }

    /// The suffix that names this operator in a key, for a message.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Lt => "lt",
            Self::Lte => "lte",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Like => "like",
            Self::Any => "in",
            Self::IsNull => "isnull",
            Self::IsNotNull => "notnull",
        }
    }
}

/// One condition, with whether it took a placeholder.
struct Clause {
    column: String,
    op: Op,
    bound: bool,
}

/// Conditions, their values, and how the rows come back.
///
/// Built by chaining and handed to a mapper:
///
/// ```ignore
/// let cheap = products
///     .find_where(
///         Query::new()
///             .eq("status", ProductStatus::Active)
///             .lt("price", Decimal::new(1000, 2))
///             .order_by("name")
///             .limit(20),
///     )
///     .await?;
/// ```
///
/// Column names are Postgres names, as the table has them. A value is
/// anything sqlx can bind for its column; getting the type wrong is a
/// database error at run time, as it would be for a hand-written query.
#[derive(Default)]
pub struct Query {
    clauses: Vec<Clause>,
    args: PgArguments,
    order: Vec<(String, bool)>,
    limit: Option<i64>,
    offset: Option<i64>,
    failed: Option<sqlx::Error>,
}

impl Query {
    /// No conditions: every row.
    pub fn new() -> Self {
        Self::default()
    }

    /// `column = value`.
    pub fn eq<'a, T>(self, column: &str, value: T) -> Self
    where
        T: 'a + sqlx::Encode<'a, Postgres> + sqlx::Type<Postgres>,
    {
        self.bind(column, Op::Eq, value)
    }

    /// `column <> value`.
    pub fn ne<'a, T>(self, column: &str, value: T) -> Self
    where
        T: 'a + sqlx::Encode<'a, Postgres> + sqlx::Type<Postgres>,
    {
        self.bind(column, Op::Ne, value)
    }

    /// `column < value`.
    pub fn lt<'a, T>(self, column: &str, value: T) -> Self
    where
        T: 'a + sqlx::Encode<'a, Postgres> + sqlx::Type<Postgres>,
    {
        self.bind(column, Op::Lt, value)
    }

    /// `column <= value`.
    pub fn lte<'a, T>(self, column: &str, value: T) -> Self
    where
        T: 'a + sqlx::Encode<'a, Postgres> + sqlx::Type<Postgres>,
    {
        self.bind(column, Op::Lte, value)
    }

    /// `column > value`.
    pub fn gt<'a, T>(self, column: &str, value: T) -> Self
    where
        T: 'a + sqlx::Encode<'a, Postgres> + sqlx::Type<Postgres>,
    {
        self.bind(column, Op::Gt, value)
    }

    /// `column >= value`.
    pub fn gte<'a, T>(self, column: &str, value: T) -> Self
    where
        T: 'a + sqlx::Encode<'a, Postgres> + sqlx::Type<Postgres>,
    {
        self.bind(column, Op::Gte, value)
    }

    /// `column LIKE value`.
    pub fn like<'a, T>(self, column: &str, value: T) -> Self
    where
        T: 'a + sqlx::Encode<'a, Postgres> + sqlx::Type<Postgres>,
    {
        self.bind(column, Op::Like, value)
    }

    /// `column = ANY(values)`.
    pub fn any<'a, T>(self, column: &str, values: Vec<T>) -> Self
    where
        Vec<T>: 'a + sqlx::Encode<'a, Postgres> + sqlx::Type<Postgres>,
    {
        self.bind(column, Op::Any, values)
    }

    /// `column IS NULL`.
    pub fn null(mut self, column: &str) -> Self {
        self.clauses.push(Clause {
            column: column.to_string(),
            op: Op::IsNull,
            bound: false,
        });
        self
    }

    /// `column IS NOT NULL`.
    pub fn not_null(mut self, column: &str) -> Self {
        self.clauses.push(Clause {
            column: column.to_string(),
            op: Op::IsNotNull,
            bound: false,
        });
        self
    }

    /// `column <op> value`, for an operator chosen at run time. The two
    /// null tests take no value and ignore the one given.
    pub fn cond<'a, T>(self, column: &str, op: Op, value: T) -> Self
    where
        T: 'a + sqlx::Encode<'a, Postgres> + sqlx::Type<Postgres>,
    {
        match op {
            Op::IsNull => self.null(column),
            Op::IsNotNull => self.not_null(column),
            op => self.bind(column, op, value),
        }
    }

    /// Order by `column`, ascending. Call again for a second key.
    pub fn order_by(mut self, column: &str) -> Self {
        self.order.push((column.to_string(), false));
        self
    }

    /// Order by `column`, descending.
    pub fn order_by_desc(mut self, column: &str) -> Self {
        self.order.push((column.to_string(), true));
        self
    }

    /// At most `n` rows.
    pub fn limit(mut self, n: i64) -> Self {
        self.limit = Some(n);
        self
    }

    /// Skip the first `n` rows.
    pub fn offset(mut self, n: i64) -> Self {
        self.offset = Some(n);
        self
    }

    /// `SELECT <every column> FROM {from}` with the clause, the order,
    /// the limit and the offset, and the arguments to run it with.
    ///
    /// `columns` is every column the query may name, as `(name,
    /// identifier)`: the Postgres name and the identifier as a statement
    /// writes it. The mapper supplies it, and it is also the select
    /// list: the statement names what the row type holds, never `*`.
    ///
    /// # Errors
    ///
    /// [`sqlx::Error::ColumnNotFound`] for a name that is not in
    /// `columns`, and [`sqlx::Error::Encode`] if a value could not be
    /// bound.
    pub fn select(
        self,
        from: &str,
        columns: &[(&str, &str)],
    ) -> Result<(String, PgArguments), sqlx::Error> {
        let list = columns
            .iter()
            .map(|(_, ident)| *ident)
            .collect::<Vec<_>>()
            .join(", ");
        self.assemble(&format!("SELECT {list} FROM {from}"), columns, true)
    }

    /// `SELECT count(*) FROM {from}` with the clause. Order, limit and
    /// offset do not apply.
    ///
    /// # Errors
    ///
    /// As [`Query::select`].
    pub fn count(
        self,
        from: &str,
        columns: &[(&str, &str)],
    ) -> Result<(String, PgArguments), sqlx::Error> {
        self.assemble(&format!("SELECT count(*) FROM {from}"), columns, false)
    }

    fn bind<'a, T>(mut self, column: &str, op: Op, value: T) -> Self
    where
        T: 'a + sqlx::Encode<'a, Postgres> + sqlx::Type<Postgres>,
    {
        match self.args.add(value) {
            Ok(()) => self.clauses.push(Clause {
                column: column.to_string(),
                op,
                bound: true,
            }),
            Err(e) => {
                if self.failed.is_none() {
                    self.failed = Some(sqlx::Error::Encode(e));
                }
            }
        }
        self
    }

    fn assemble(
        mut self,
        head: &str,
        columns: &[(&str, &str)],
        paged: bool,
    ) -> Result<(String, PgArguments), sqlx::Error> {
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        let quoted = |name: &str| -> Result<&str, sqlx::Error> {
            columns
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, q)| *q)
                .ok_or_else(|| sqlx::Error::ColumnNotFound(name.to_string()))
        };

        let mut sql = head.to_string();
        let mut n = 0;
        for (i, clause) in self.clauses.iter().enumerate() {
            sql.push_str(if i == 0 { " WHERE " } else { " AND " });
            sql.push_str(quoted(&clause.column)?);
            if clause.bound {
                n += 1;
            }
            sql.push_str(&match clause.op {
                Op::Eq => format!(" = ${n}"),
                Op::Ne => format!(" <> ${n}"),
                Op::Lt => format!(" < ${n}"),
                Op::Lte => format!(" <= ${n}"),
                Op::Gt => format!(" > ${n}"),
                Op::Gte => format!(" >= ${n}"),
                Op::Like => format!(" LIKE ${n}"),
                Op::Any => format!(" = ANY(${n})"),
                Op::IsNull => " IS NULL".to_string(),
                Op::IsNotNull => " IS NOT NULL".to_string(),
            });
        }
        if paged {
            for (i, (column, desc)) in self.order.iter().enumerate() {
                sql.push_str(if i == 0 { " ORDER BY " } else { ", " });
                sql.push_str(quoted(column)?);
                if *desc {
                    sql.push_str(" DESC");
                }
            }
            if let Some(limit) = self.limit {
                n += 1;
                self.args.add(limit).map_err(sqlx::Error::Encode)?;
                sql.push_str(&format!(" LIMIT ${n}"));
            }
            if let Some(offset) = self.offset {
                n += 1;
                self.args.add(offset).map_err(sqlx::Error::Encode)?;
                sql.push_str(&format!(" OFFSET ${n}"));
            }
        }
        Ok((sql, self.args))
    }
}
