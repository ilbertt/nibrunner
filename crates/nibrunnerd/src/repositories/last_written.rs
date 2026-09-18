use std::collections::BTreeMap;
use std::future::Future;

use tokio::sync::{Mutex, MutexGuard};

use crate::domain::store::StoreError;

/// What a repository last wrote, so that a pass writes the rows it changed rather than the table.
///
/// Unknown until the first write, which reads the table first: what an earlier daemon left is
/// diffed against rather than written over. Only a committed write is remembered — one that
/// failed is forgotten whatever it left behind, and the next reads the table over again.
pub(super) struct LastWritten<T>(Mutex<Option<T>>);

impl<T: PartialEq> LastWritten<T> {
    pub(super) fn unknown() -> Self {
        Self(Mutex::new(None))
    }

    /// The write that takes the table from what it holds to `wanted`; `held` is read only when
    /// nothing is remembered. Held until `written` or dropped, so two passes cannot diff against
    /// the same memory.
    pub(super) async fn towards<F>(&self, wanted: T, held: F) -> Result<Delta<'_, T>, StoreError>
    where
        F: Future<Output = Result<T, StoreError>>,
    {
        let mut memory = self.0.lock().await;
        let last = match memory.take() {
            Some(last) => last,
            None => held.await?,
        };
        Ok(Delta { memory, last, wanted })
    }
}

pub(super) struct Delta<'a, T> {
    memory: MutexGuard<'a, Option<T>>,
    last: T,
    pub(super) wanted: T,
}

impl<T: PartialEq> Delta<'_, T> {
    pub(super) fn is_empty(&self) -> bool {
        self.last == self.wanted
    }

    /// Once the transaction is in: what was wanted is what was last written.
    pub(super) fn written(self) {
        let Delta {
            mut memory, wanted, ..
        } = self;
        *memory = Some(wanted);
    }
}

impl<V: PartialEq> Delta<'_, BTreeMap<String, V>> {
    /// The rows of `wanted` the table does not hold, or holds with another value.
    pub(super) fn changed(&self) -> impl Iterator<Item = (&str, &V)> + '_ {
        self.wanted
            .iter()
            .filter(|(key, value)| self.last.get(*key) != Some(*value))
            .map(|(key, value)| (key.as_str(), value))
    }

    /// The rows the table holds that `wanted` does not.
    pub(super) fn gone(&self) -> impl Iterator<Item = &str> + '_ {
        self.last
            .keys()
            .filter(|key| !self.wanted.contains_key(*key))
            .map(String::as_str)
    }
}

/// Every row a table is told to insert, update or delete, so a test can say what a pass wrote
/// and not only what it left behind.
#[cfg(test)]
pub(super) mod written {
    use sqlx::SqlitePool;

    pub(crate) async fn watch(pool: &SqlitePool, table: &str, key: &str) {
        sqlx::query("create table if not exists written (op text not null, key text not null)")
            .execute(pool)
            .await
            .unwrap();
        for (op, row) in [("insert", "new"), ("update", "new"), ("delete", "old")] {
            sqlx::query(&format!(
                "create trigger {table}_{op}s after {op} on {table} \
                 begin insert into written (op, key) values ('{op}', {row}.{key}); end"
            ))
            .execute(pool)
            .await
            .unwrap();
        }
    }

    /// What was written since this was last asked, in the order it was, as `insert app-1`.
    pub(crate) async fn writes(pool: &SqlitePool) -> Vec<String> {
        let rows: Vec<(String, String)> = sqlx::query_as("select op, key from written order by rowid")
            .fetch_all(pool)
            .await
            .unwrap();
        sqlx::query("delete from written").execute(pool).await.unwrap();
        rows.into_iter().map(|(op, key)| format!("{op} {key}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(rows: &[(&str, i64)]) -> BTreeMap<String, i64> {
        rows.iter()
            .map(|(key, value)| (key.to_string(), *value))
            .collect()
    }

    async fn from_table(rows: BTreeMap<String, i64>) -> Result<BTreeMap<String, i64>, StoreError> {
        Ok(rows)
    }

    async fn unread() -> Result<BTreeMap<String, i64>, StoreError> {
        panic!("the table is read only once")
    }

    #[tokio::test]
    async fn the_first_write_diffs_against_the_table_rather_than_against_nothing() {
        let memory = LastWritten::unknown();
        let delta = memory
            .towards(
                held(&[("a", 1), ("b", 2)]),
                from_table(held(&[("a", 1), ("c", 3)])),
            )
            .await
            .unwrap();
        assert_eq!(delta.changed().collect::<Vec<_>>(), vec![("b", &2)]);
        assert_eq!(delta.gone().collect::<Vec<_>>(), vec!["c"]);
    }

    #[tokio::test]
    async fn a_committed_write_is_what_the_next_one_diffs_against() {
        let memory = LastWritten::unknown();
        memory
            .towards(held(&[("a", 1)]), from_table(BTreeMap::new()))
            .await
            .unwrap()
            .written();
        let delta = memory.towards(held(&[("a", 2)]), unread()).await.unwrap();
        assert_eq!(delta.changed().collect::<Vec<_>>(), vec![("a", &2)]);
        assert!(delta.gone().next().is_none());
    }

    #[tokio::test]
    async fn a_write_that_was_not_committed_is_not_remembered() {
        let memory = LastWritten::unknown();
        drop(
            memory
                .towards(held(&[("a", 1)]), from_table(BTreeMap::new()))
                .await
                .unwrap(),
        );
        let delta = memory
            .towards(held(&[("a", 1)]), from_table(BTreeMap::new()))
            .await
            .unwrap();
        assert_eq!(delta.changed().collect::<Vec<_>>(), vec![("a", &1)]);
    }

    #[tokio::test]
    async fn a_row_worth_the_same_is_not_a_change() {
        let memory = LastWritten::unknown();
        let delta = memory
            .towards(held(&[("a", 1)]), from_table(held(&[("a", 1)])))
            .await
            .unwrap();
        assert!(delta.is_empty());
        assert!(delta.changed().next().is_none());
    }
}
