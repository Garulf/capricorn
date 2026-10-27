use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, STORED, STRING, Schema, TEXT, TextFieldIndexing,
    TextOptions,
};
use tantivy::tokenizer::{LowerCaser, RawTokenizer, TextAnalyzer};
use tantivy::{Index as TantivyIndex, directory::RamDirectory};

use anyhow::Result;

const RAW_LOWERCASE: &str = "raw_lc";

#[derive(Clone, Copy)]
pub(crate) struct Fields {
    pub record_ref: Field,
    pub hour: Field,
    pub ts: Field,
    pub kind: Field,
    pub app: Field,
    pub app_key: Field,
    pub app_raw: Field,
    pub title: Field,
    pub workspace: Field,
    pub cwd: Field,
    pub url: Field,
    pub body: Field,
}

fn fast_string() -> TextOptions {
    STRING.set_stored().set_fast("raw")
}

fn case_insensitive_key() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(RAW_LOWERCASE)
            .set_index_option(IndexRecordOption::Basic),
    )
}

fn fast_indexed_number() -> NumericOptions {
    NumericOptions::default().set_indexed().set_fast()
}

pub(crate) fn build() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let fields = Fields {
        record_ref: builder.add_u64_field("ref", fast_indexed_number().set_stored()),
        hour: builder.add_u64_field("hour", fast_indexed_number()),
        ts: builder.add_i64_field("ts", fast_indexed_number().set_stored()),
        kind: builder.add_text_field("kind", fast_string()),
        app: builder.add_text_field("app", TEXT),
        app_key: builder.add_text_field("app_key", case_insensitive_key()),
        app_raw: builder.add_text_field("app_raw", fast_string()),
        title: builder.add_text_field("title", TEXT | STORED),
        workspace: builder.add_text_field("workspace", STRING | STORED),
        cwd: builder.add_text_field("cwd", TEXT | STORED),
        url: builder.add_text_field("url", TEXT | STORED),
        body: builder.add_text_field("body", TEXT | STORED),
    };
    (builder.build(), fields)
}

pub(crate) fn register_tokenizers(index: &TantivyIndex) {
    let analyzer = TextAnalyzer::builder(RawTokenizer::default())
        .filter(LowerCaser)
        .build();
    index.tokenizers().register(RAW_LOWERCASE, analyzer);
}

pub(crate) fn new_in_ram() -> Result<(TantivyIndex, RamDirectory, Fields)> {
    let (schema, fields) = build();
    let directory = RamDirectory::create();
    let index = TantivyIndex::create(directory.clone(), schema, tantivy::IndexSettings::default())?;
    register_tokenizers(&index);
    Ok((index, directory, fields))
}
