use distributary::{ControllerHandle, DataType, Mutator, RemoteGetter, RpcError, ZookeeperAuthority};
use msql_srv::{self, *};
use nom_sql;
use slog;
use std::io;
use std::collections::BTreeMap;

pub struct SoupBackend {
    soup: ControllerHandle<ZookeeperAuthority>,
    log: slog::Logger,

    _recipe: String,
    inputs: BTreeMap<String, Mutator>,
    outputs: BTreeMap<String, RemoteGetter>,

    query_count: u64,
}

impl SoupBackend {
    pub fn new(log: slog::Logger) -> Self {
        let mut zk_auth = ZookeeperAuthority::new("127.0.0.1:2181");
        zk_auth.log_with(log.clone());

        debug!(log, "Connecting to Soup...",);
        let mut ch = ControllerHandle::new(zk_auth);

        let inputs = ch.inputs()
            .into_iter()
            .map(|(n, _)| (n.clone(), ch.get_mutator(&n).unwrap()))
            .collect::<BTreeMap<String, Mutator>>();
        let outputs = ch.outputs()
            .into_iter()
            .map(|(n, _)| (n.clone(), ch.get_getter(&n).unwrap()))
            .collect::<BTreeMap<String, RemoteGetter>>();

        debug!(log, "Connected!");

        SoupBackend {
            soup: ch,
            log: log,

            _recipe: String::new(),
            inputs: inputs,
            outputs: outputs,

            query_count: 0,
        }
    }

    fn handle_create_table<W: io::Write>(
        &mut self,
        q: &str,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        match self.soup.extend_recipe(format!("{}", q)) {
            Ok(_) => {
                // no rows to return
                results.completed(0, 0)
            }
            Err(e) => {
                // XXX(malte): implement Error for RpcError
                let msg = match e {
                    RpcError::Other(msg) => msg,
                };
                Err(io::Error::new(io::ErrorKind::Other, msg))
            }
        }
    }

    fn handle_insert<W: io::Write>(
        &mut self,
        q: nom_sql::InsertStatement,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        let table = q.table.name.clone();

        // create a getter if we don't have only for this table already
        // TODO(malte): may need to make one anyway if the query has changed w.r.t. an
        // earlier one of the same name?
        let putter = self.inputs
            .entry(table.clone())
            .or_insert(self.soup.get_mutator(&table).unwrap());

        match putter.put(
            q.fields
                .into_iter()
                .map(|(_, v)| DataType::from(v))
                .collect::<Vec<DataType>>(),
        ) {
            Ok(_) => Ok(()),
            Err(_) => results.error(msql_srv::ErrorKind::ER_PARSE_ERROR, "".as_bytes()),
        }
    }

    fn handle_select<W: io::Write>(
        &mut self,
        q: nom_sql::SelectStatement,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        let qname = format!("q_{}", self.query_count);

        // first do a migration to add the query if it doesn't exist already
        match self.soup.extend_recipe(format!("QUERY {}: {};", qname, q)) {
            Ok(_) => {
                self.query_count += 1;

                // create a getter if we don't have only for this table already
                // TODO(malte): may need to make one anyway if the query has changed w.r.t. an
                // earlier one of the same name?
                let getter = self.outputs
                    .entry(qname.clone())
                    .or_insert(self.soup.get_getter(&qname).unwrap());

                // now "execute" the query via a bogokey lookup
                match getter.lookup(&DataType::None, true) {
                    Ok(_) => results.completed(0, 0),
                    Err(_) => results.error(msql_srv::ErrorKind::ER_NO, "".as_bytes()),
                }
            }
            Err(e) => {
                // XXX(malte): implement Error for RpcError
                let msg = match e {
                    RpcError::Other(msg) => msg,
                };
                Err(io::Error::new(io::ErrorKind::Other, msg))
            }
        }
    }

    fn handle_set<W: io::Write>(
        &mut self,
        _q: nom_sql::SetStatement,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        // ignore
        results.completed(0, 0)
    }
}

impl<W: io::Write> MysqlShim<W> for SoupBackend {
    fn on_prepare(&mut self, query: &str, info: StatementMetaWriter<W>) -> io::Result<()> {
        error!(self.log, "prepare: {}", query);
        info.reply(42, &[], &[])
    }

    fn on_execute(
        &mut self,
        id: u32,
        _: ParamParser,
        results: QueryResultWriter<W>,
    ) -> io::Result<()> {
        error!(self.log, "exec: {}", id);
        results.completed(0, 0)
    }

    fn on_close(&mut self, _: u32) {}

    fn on_query(&mut self, query: &str, results: QueryResultWriter<W>) -> io::Result<()> {
        debug!(self.log, "query: {}", query);

        if query.to_lowercase().contains("show tables") || query.to_lowercase().contains("rollback")
        {
            return results.completed(0, 0);
        }

        match nom_sql::parse_query(query) {
            Ok(q) => match q {
                nom_sql::SqlQuery::CreateTable(_) => self.handle_create_table(query, results),
                nom_sql::SqlQuery::Insert(q) => self.handle_insert(q, results),
                nom_sql::SqlQuery::Select(q) => self.handle_select(q, results),
                nom_sql::SqlQuery::Set(q) => self.handle_set(q, results),
                _ => {
                    return results.error(
                        msql_srv::ErrorKind::ER_NOT_SUPPORTED_YET,
                        "unsupported query".as_bytes(),
                    )
                }
            },
            Err(e) => {
                // if nom-sql rejects the query, there is no chance Soup will like it
                crit!(self.log, "query can't be parsed: \"{}\"", query);
                return results.error(msql_srv::ErrorKind::ER_PARSE_ERROR, e.as_bytes());
            }
        }
    }
}
