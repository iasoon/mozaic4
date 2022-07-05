use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::schema::{bot_versions, bots};
use chrono;

#[derive(Insertable)]
#[table_name = "bots"]
pub struct NewBot<'a> {
    pub owner_id: Option<i32>,
    pub name: &'a str,
}

#[derive(Queryable, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bot {
    pub id: i32,
    pub owner_id: Option<i32>,
    pub name: String,
}

pub fn create_bot(new_bot: &NewBot, conn: &PgConnection) -> QueryResult<Bot> {
    diesel::insert_into(bots::table)
        .values(new_bot)
        .get_result(conn)
}

pub fn find_bot(id: i32, conn: &PgConnection) -> QueryResult<Bot> {
    bots::table.find(id).first(conn)
}

pub fn find_bots_by_owner(owner_id: i32, conn: &PgConnection) -> QueryResult<Vec<Bot>> {
    bots::table
        .filter(bots::owner_id.eq(owner_id))
        .get_results(conn)
}

pub fn find_bot_by_name(name: &str, conn: &PgConnection) -> QueryResult<Bot> {
    bots::table.filter(bots::name.eq(name)).first(conn)
}

pub fn find_all_bots(conn: &PgConnection) -> QueryResult<Vec<Bot>> {
    // TODO: filter out bots that cannot be run (have no valid code bundle associated with them)
    bots::table.get_results(conn)
}

#[derive(Insertable)]
#[table_name = "bot_versions"]
pub struct NewCodeBundle<'a> {
    pub bot_id: Option<i32>,
    pub code_bundle_path: &'a str,
}

#[derive(Queryable, Serialize, Deserialize, Debug)]
pub struct CodeBundle {
    pub id: i32,
    pub bot_id: Option<i32>,
    pub code_bundle_path: Option<String>,
    pub created_at: chrono::NaiveDateTime,
    pub container_digest: Option<String>,
}

pub fn create_code_bundle(
    new_code_bundle: &NewCodeBundle,
    conn: &PgConnection,
) -> QueryResult<CodeBundle> {
    diesel::insert_into(bot_versions::table)
        .values(new_code_bundle)
        .get_result(conn)
}

pub fn find_bot_code_bundles(bot_id: i32, conn: &PgConnection) -> QueryResult<Vec<CodeBundle>> {
    bot_versions::table
        .filter(bot_versions::bot_id.eq(bot_id))
        .get_results(conn)
}

pub fn active_code_bundle(bot_id: i32, conn: &PgConnection) -> QueryResult<CodeBundle> {
    bot_versions::table
        .filter(bot_versions::bot_id.eq(bot_id))
        .order(bot_versions::created_at.desc())
        .first(conn)
}
