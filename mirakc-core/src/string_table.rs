use std::fs::File;
use std::sync::Arc;

use serde::Deserialize;

pub fn load(path: &str) -> Arc<StringTable> {
    let reader = File::open(path).unwrap_or_else(|err| panic!("Failed to open {}: {}", path, err));
    let string_table: StringTable = serde_yml::from_reader(reader)
        .unwrap_or_else(|err| panic!("Failed to parse {}: {}", path, err));
    Arc::new(string_table)
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(Debug))]
pub struct StringTable {
    pub genres: [EpgGenreStringTable; 16],
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(Debug))]
pub struct EpgGenreStringTable {
    pub genre: String,
    pub subgenres: [String; 16],
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_log::test;

    #[test]
    fn test_epg_genre_string_table() {
        let result = serde_yml::from_str::<EpgGenreStringTable>("{}");
        assert!(result.is_err());

        assert_eq!(
            serde_yml::from_str::<EpgGenreStringTable>(
                r#"
                genre: 'name'
                subgenres:
                  - sub1
                  - sub2
                  - sub3
                  - sub4
                  - sub5
                  - sub6
                  - sub7
                  - sub8
                  - sub9
                  - sub10
                  - sub11
                  - sub12
                  - sub13
                  - sub14
                  - sub15
                  - sub16
            "#
            )
            .unwrap(),
            EpgGenreStringTable {
                genre: "name".to_string(),
                subgenres: [
                    "sub1".to_string(),
                    "sub2".to_string(),
                    "sub3".to_string(),
                    "sub4".to_string(),
                    "sub5".to_string(),
                    "sub6".to_string(),
                    "sub7".to_string(),
                    "sub8".to_string(),
                    "sub9".to_string(),
                    "sub10".to_string(),
                    "sub11".to_string(),
                    "sub12".to_string(),
                    "sub13".to_string(),
                    "sub14".to_string(),
                    "sub15".to_string(),
                    "sub16".to_string(),
                ],
            }
        );
    }
}
