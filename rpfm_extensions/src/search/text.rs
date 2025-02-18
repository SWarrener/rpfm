//---------------------------------------------------------------------------//
// Copyright (c) 2017-2023 Ismael Gutiérrez González. All rights reserved.
//
// This file is part of the Rusted PackFile Manager (RPFM) project,
// which can be found here: https://github.com/Frodo45127/rpfm.
//
// This file is licensed under the MIT license, which can be found here:
// https://github.com/Frodo45127/rpfm/blob/master/LICENSE.
//---------------------------------------------------------------------------//

/*!
Module with all the code related to the `TextMatches`.

This module contains the code needed to get text matches from a `GlobalSearch`.
!*/

use getset::{Getters, MutGetters};
use itertools::Itertools;

use rpfm_lib::files::text::Text;

use super::{MatchingMode, Replaceable, Searchable};

//-------------------------------------------------------------------------------//
//                              Enums & Structs
//-------------------------------------------------------------------------------//

/// This struct represents all the matches of the global search within a text PackedFile.
#[derive(Debug, Clone, Getters, MutGetters)]
#[getset(get = "pub", get_mut = "pub")]
pub struct TextMatches {

    /// The path of the file.
    path: String,

    /// The list of matches within the file.
    matches: Vec<TextMatch>,
}

/// This struct represents a match on a piece of text within a Text PackedFile.
#[derive(Debug, Clone, Eq, PartialEq, Getters, MutGetters)]
#[getset(get = "pub", get_mut = "pub")]
pub struct TextMatch {

    /// Column of the first character of the match.
    column: u64,

    /// Row of the first character of the match.
    row: u64,

    /// Length of the matched pattern.
    len: i64,

    /// Line of text containing the match.
    text: String,
}

//-------------------------------------------------------------------------------//
//                             Implementations
//-------------------------------------------------------------------------------//

impl Searchable for Text {
    type SearchMatches = TextMatches;

    fn search(&self, file_path: &str, pattern: &str, case_sensitive: bool, matching_mode: &MatchingMode) -> TextMatches {

        // TODO: while it searches quite fast... I think it can be improved even more.
        let mut matches = TextMatches::new(file_path);
        match matching_mode {
            MatchingMode::Regex(regex) => {
                for (row, data) in self.contents().lines().enumerate() {
                    for match_data in regex.find_iter(data) {
                        matches.matches.push(
                            TextMatch::new(
                                match_data.start() as u64,
                                row as u64,
                                (match_data.end() - match_data.start()) as i64,
                                data.to_owned()
                            )
                        );
                    }
                }
            }

            // If we're searching a pattern, we just check every text PackedFile, line by line.
            MatchingMode::Pattern => {
                let length = pattern.len();
                let mut column = 0;

                for (row, data) in self.contents().lines().enumerate() {
                    while let Some(text) = data.get(column..) {
                        if case_sensitive {
                            match text.find(pattern) {
                                Some(position) => {
                                    matches.matches.push(TextMatch::new(column as u64 + position as u64, row as u64, length as i64, data.to_owned()));
                                    column += position + length;
                                }
                                None => break,
                            }
                        }
                        else {
                            let text = text.to_lowercase();
                            match text.find(pattern) {
                                Some(position) => {
                                    matches.matches.push(TextMatch::new(column as u64 + position as u64, row as u64, length as i64, data.to_owned()));
                                    column += position + length;
                                }
                                None => break,
                            }
                        }
                    }

                    column = 0;
                }
            }
        }

        matches
    }
}

impl Replaceable for Text {

    fn replace(&mut self, _pattern: &str, replace_pattern: &str, _case_sensitive: bool, _matching_mode: &MatchingMode, search_matches: &TextMatches) -> bool {
        let mut edited = false;

        // NOTE: Due to changes in index positions, we need to do this in reverse.
        // Otherwise we may cause one edit to generate invalid indexes for the next matches.
        for search_match in search_matches.matches().iter().rev() {
            edited |= search_match.replace(replace_pattern, self.contents_mut());
        }

        edited
    }
}

impl TextMatches {

    /// This function creates a new `TextMatches` for the provided path.
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            matches: vec![],
        }
    }
}

impl TextMatch {

    /// This function creates a new `TextMatch` with the provided data.
    pub fn new(column: u64, row: u64, len: i64, text: String) -> Self {
        Self {
            column,
            row,
            len,
            text,
        }
    }

    /// This function replaces all the matches in the provided text.
    fn replace(&self, replace_pattern: &str, data: &mut String) -> bool {
        let mut edited = false;

        let new_data = data.lines()
            .enumerate()
            .map(|(row, line)| {
                if self.row == row as u64 {
                    let mut new_line = line.to_owned();
                    new_line.replace_range(self.column as usize..self.column as usize + self.len as usize, replace_pattern);
                    new_line
                } else {
                    line.to_owned()
                }
            }).join("\n");

        if new_data != *data {
            *data = new_data;
            edited = true;
        }

        edited
    }
}
