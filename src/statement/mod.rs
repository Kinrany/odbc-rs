
mod types;
mod bind_parameter;
pub use self::types::Output;
use {ffi, DataSource, Return, Result, Raii, Handle, Connected};
use ffi::SQLRETURN::*;
use std::marker::PhantomData;

/// `Statement` state used to represent a freshly allocated connection
pub enum Allocated {}
/// `Statement` state used to represent a statement with a result set cursor
///
/// A statement is most likely to enter this state after a `SELECT` query.
pub enum HasResult {}
/// `Statement` state used to represent a statement with no result set
///
/// A statement is likely to enter this state after executing e.g. a `CREATE TABLE` statement
type NoResult = Allocated; // pub enum NoResult {}

/// Holds a `Statement` after execution of a query.Allocated
///
/// A executed statement may be in one of two states. Either the statement has yielded a result set
/// or not. Keep in mind that some ODBC drivers just yield empty result sets on e.g. `INSERT`
/// Statements
pub enum Executed<'a, 'b> {
    Data(Statement<'a, 'b, HasResult>),
    NoData(Statement<'a, 'b, NoResult>),
}
pub use Executed::*;

/// RAII wrapper around ODBC statement
pub struct Statement<'a, 'b, S> {
    raii: Raii<ffi::Stmt>,
    // we use phantom data to tell the borrow checker that we need to keep the data source alive
    // for the lifetime of the statement
    parent: PhantomData<&'a DataSource<'a, Connected>>,
    state: PhantomData<S>,
    bound: PhantomData<&'b [u8]>,
}

/// Used to retrieve data from the fields of a query resul
pub struct Cursor<'a, 'b: 'a, 'c : 'a> {
    stmt: &'a mut Statement<'b, 'c, HasResult>,
    buffer: [u8; 512],
}

impl<'a, 'b, S> Handle for Statement<'a, 'b, S> {
    type To = ffi::Stmt;
    unsafe fn handle(&self) -> ffi::SQLHSTMT {
        self.raii.handle()
    }
}

impl<'a, 'b, S> Statement<'a, 'b, S> {
    fn with_raii(raii: Raii<ffi::Stmt>) -> Self {
        Statement {
            raii: raii,
            parent: PhantomData,
            state: PhantomData,
            bound: PhantomData,
        }
    }
}

impl<'a, 'b> Statement<'a, 'b, Allocated> {
    pub fn with_parent(ds: &'a DataSource<Connected>) -> Result<Self> {
        let raii = Raii::with_parent(ds).into_result(ds)?;
        Ok(Self::with_raii(raii))
    }

    pub fn tables(mut self) -> Result<Statement<'a, 'b, HasResult>> {
        self.raii.tables().into_result(&self)?;
        Ok(Statement::with_raii(self.raii))
    }

    /// Executes a preparable statement, using the current values of the parameter marker variables
    /// if any parameters exist in the statement.
    ///
    /// `SQLExecDirect` is the fastest way to submit an SQL statement for one-time execution.
    pub fn exec_direct(mut self, statement_text: &str) -> Result<Executed<'a, 'b>> {
        if self.raii.exec_direct(statement_text).into_result(&self)? {
            Ok(Executed::Data(Statement::with_raii(self.raii)))
        } else {
            Ok(Executed::NoData(Statement::with_raii(self.raii)))
        }
    }
}

impl<'a, 'b> Statement<'a, 'b, HasResult> {
    /// The number of columns in a result set
    ///
    /// Can be called successfully only when the statement is in the prepared, executed, or
    /// positioned state. If the statement does not return columns the result will be 0.
    pub fn num_result_cols(&self) -> Result<i16> {
        self.raii.num_result_cols().into_result(self)
    }

    /// Fetches the next rowset of data from the result set and returns data for all bound columns.
    ///
    /// # Return
    /// Returns false on the last row
    pub fn fetch<'c>(&'c mut self) -> Result<Option<Cursor<'c, 'a, 'b>>> {
        if self.raii.fetch().into_result(self)? {
            Ok(Some(Cursor {
                stmt: self,
                buffer: [0u8; 512],
            }))
        } else {
            Ok(None)
        }
    }

    /// Call this method to reuse the statement to execute another query.
    ///
    /// For many drivers allocating new statemens is expensive. So reusing a `Statement` is usually
    /// more efficient than freeing an existing and alloctaing a new one. However to reuse a
    /// statement any open result sets must be closed.
    /// Only call this method if you have already read the result set returned by the previous
    /// query, or if you do no not intend to read it.
    ///
    /// # Example
    ///
    /// ```
    /// # use odbc::*;
    /// # fn reuse () -> Result<()> {
    /// let env = Environment::new().unwrap().set_odbc_version_3()?;
    /// let conn = DataSource::with_parent(&env)?.connect("TestDataSource", "", "")?;
    /// let stmt = Statement::with_parent(&conn)?;
    /// let stmt = match stmt.exec_direct("CREATE TABLE STAGE (A TEXT, B TEXT);")?{
    ///     // Some drivers will return an empty result set. We need to close it before we can use
    ///     // statement again.
    ///     Data(stmt) => stmt.close_cursor()?,
    ///     NoData(stmt) => stmt,
    /// };
    /// let stmt = stmt.exec_direct("INSERT INTO STAGE (A, B) VALUES ('Hello', 'World');")?;
    /// //...
    /// # Ok(())
    /// # };
    /// ```
    pub fn close_cursor(mut self) -> Result<Statement<'a, 'b, NoResult>> {
        self.raii.close_cursor().into_result(&self)?;
        Ok(Statement::with_raii(self.raii))
    }
}

impl<'a, 'b, 'c> Cursor<'a, 'b, 'c> {
    /// Retrieves data for a single column in the result set
    pub fn get_data<'d, T>(&'d mut self, col_or_param_num: u16) -> Result<Option<T>>
        where T: Output<'d>
    {
        T::get_data(&mut self.stmt.raii, col_or_param_num, &mut self.buffer).into_result(self.stmt)
    }
}

impl Raii<ffi::Stmt> {
    fn num_result_cols(&self) -> Return<i16> {
        let mut num_cols: ffi::SQLSMALLINT = 0;
        unsafe {
            match ffi::SQLNumResultCols(self.handle(), &mut num_cols as *mut ffi::SQLSMALLINT) {
                SQL_SUCCESS => Return::Success(num_cols),
                SQL_SUCCESS_WITH_INFO => Return::SuccessWithInfo(num_cols),
                SQL_ERROR => Return::Error,
                r => panic!("SQLNumResultCols returned unexpected result: {:?}", r),
            }
        }
    }

    fn exec_direct(&mut self, statement_text: &str) -> Return<bool> {
        let length = statement_text.len();
        if length > ffi::SQLINTEGER::max_value() as usize {
            panic!("Statement text too long");
        }
        match unsafe {
            ffi::SQLExecDirect(self.handle(),
                               statement_text.as_ptr(),
                               length as ffi::SQLINTEGER)
        } {
            ffi::SQL_SUCCESS => Return::Success(true),
            ffi::SQL_SUCCESS_WITH_INFO => Return::SuccessWithInfo(true),
            ffi::SQL_ERROR => Return::Error,
            ffi::SQL_NEED_DATA => panic!("SQLExecDirec returned SQL_NEED_DATA"),
            ffi::SQL_NO_DATA => Return::Success(false),
            r => panic!("SQLExecDirect returned unexpected result: {:?}", r),
        }
    }

    /// Fetches the next rowset of data from the result set and returns data for all bound columns.
    fn fetch(&mut self) -> Return<bool> {
        match unsafe { ffi::SQLFetch(self.handle()) } {
            ffi::SQL_SUCCESS => Return::Success(true),
            ffi::SQL_SUCCESS_WITH_INFO => Return::SuccessWithInfo(true),
            ffi::SQL_ERROR => Return::Error,
            ffi::SQL_NO_DATA => Return::Success(false),
            r => panic!("SQLFetch returned unexpected result: {:?}", r),
        }
    }

    fn tables(&mut self) -> Return<()> {
        let catalog_name = "";
        let schema_name = "";
        let table_name = "";
        let table_type = "TABLE";
        unsafe {
            match ffi::SQLTables(self.handle(),
                                 catalog_name.as_ptr(),
                                 catalog_name.as_bytes().len() as ffi::SQLSMALLINT,
                                 schema_name.as_ptr(),
                                 schema_name.as_bytes().len() as ffi::SQLSMALLINT,
                                 table_name.as_ptr(),
                                 table_name.as_bytes().len() as ffi::SQLSMALLINT,
                                 table_type.as_ptr(),
                                 table_type.as_bytes().len() as ffi::SQLSMALLINT) {
                SQL_SUCCESS => Return::Success(()),
                SQL_SUCCESS_WITH_INFO => Return::SuccessWithInfo(()),
                SQL_ERROR => Return::Error,
                r => panic!("SQLTables returned: {:?}", r),
            }
        }
    }

    fn close_cursor(&mut self) -> Return<()> {
        unsafe {
            match ffi::SQLCloseCursor(self.handle()) {
                ffi::SQL_SUCCESS => Return::Success(()),
                ffi::SQL_SUCCESS_WITH_INFO => Return::SuccessWithInfo(()),
                ffi::SQL_ERROR => Return::Error,
                r => panic!("unexpected return value from SQLCloseCursor: {:?}", r),
            }
        }
    }
}
