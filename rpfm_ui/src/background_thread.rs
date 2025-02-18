//---------------------------------------------------------------------------//
// Copyright (c) 2017-2023 Ismael Gutiérrez González. All rights reserved.
//
// This file is part of the Rusted PackFile Manager (RPFM) project,
// which can be found here: https://github.com/Frodo45127/rpfm.
//
// This file is licensed under the MIT license, &which can be &found here:
// https://github.com/Frodo45127/rpfm/blob/master/LICENSE.
//---------------------------------------------------------------------------//

/*!
Module with the background loop.

Basically, this does the heavy load of the program.
!*/

use anyhow::{anyhow, Result};
use crossbeam::channel::Sender;
use itertools::Itertools;
use open::that;
use rayon::prelude::*;
use time::OffsetDateTime;

use std::collections::{BTreeMap, HashMap, hash_map::DefaultHasher};
#[cfg(feature = "enable_tools")] use std::collections::HashSet;
use std::env::temp_dir;
use std::fs::{DirBuilder, File};
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::Ordering, RwLock};
use std::time::SystemTime;
use std::thread;

use rpfm_extensions::dependencies::Dependencies;
use rpfm_extensions::diagnostics::Diagnostics;
use rpfm_extensions::optimizer::OptimizableContainer;

use rpfm_lib::files::{animpack::AnimPack, Container, ContainerPath, db::DB, DecodeableExtraData, FileType, loc::Loc, pack::*, portrait_settings::PortraitSettings, RFile, RFileDecoded, text::*};
use rpfm_lib::games::{GameInfo, LUA_REPO, LUA_BRANCH, LUA_REMOTE, pfh_file_type::PFHFileType};
use rpfm_lib::integrations::{assembly_kit::*, git::*, log::*};
use rpfm_lib::schema::*;
use rpfm_lib::utils::*;

use rpfm_ui_common::clone;
use rpfm_ui_common::FULL_DATE_FORMAT;
use rpfm_ui_common::locale::tr;
use rpfm_ui_common::PROGRAM_PATH;

use crate::app_ui::NewFile;
use crate::{backend::*, SENTRY_GUARD};
use crate::CENTRAL_COMMAND;
use crate::communications::{CentralCommand, Command, Response, THREADS_COMMUNICATION_ERROR};
use crate::FIRST_GAME_CHANGE_DONE;
use crate::GAME_SELECTED;
use crate::initialize_pack_settings;
use crate::packedfile_views::DataSource;
use crate::SCHEMA;
use crate::settings_ui::backend::*;
use crate::SUPPORTED_GAMES;
use crate::utils::initialize_encodeable_extra_data;

/// This is the background loop that's going to be executed in a parallel thread to the UI. No UI or "Unsafe" stuff here.
///
/// All communication between this and the UI thread is done use the `CENTRAL_COMMAND` static.
pub fn background_loop() {

    //---------------------------------------------------------------------------------------//
    // Initializing stuff...
    //---------------------------------------------------------------------------------------//

    // We need two PackFiles:
    // - `pack_file_decoded`: This one will hold our opened PackFile.
    // - `pack_files_decoded_extra`: This one will hold the PackFiles opened for the `add_from_packfile` feature, using their paths as keys.
    let mut pack_file_decoded = Pack::default();
    let mut pack_files_decoded_extra = BTreeMap::new();

    // Preload the default game's dependencies.
    let mut dependencies = Arc::new(RwLock::new(Dependencies::default()));

    // Initalize background sentry guard. This should, in theory, register crashes on the background thread.
    let _sentry_guard = Logger::init(&error_path().unwrap_or_else(|_| PathBuf::from(".")), true, false);

    // Load all the tips we have.
    //let mut tips = if let Ok(tips) = Tips::load() { tips } else { Tips::default() };

    //---------------------------------------------------------------------------------------//
    // Looping forever and ever...
    //---------------------------------------------------------------------------------------//
    info!("Background Thread looping around…");
    'background_loop: loop {

        // Wait until you get something through the channel. This hangs the thread until we got something,
        // so it doesn't use processing power until we send it a message.
        let (sender, response): (Sender<Response>, Command) = CENTRAL_COMMAND.recv_background();
        match response {

            // Command to close the thread.
            Command::Exit => return,

            // In case we want to reset the PackFile to his original state (dummy)...
            Command::ResetPackFile => pack_file_decoded = Pack::default(),

            // In case we want to remove a Secondary Packfile from memory...
            Command::RemovePackFileExtra(path) => { pack_files_decoded_extra.remove(&path); },

            // In case we want to create a "New PackFile"...
            Command::NewPackFile => {
                let game_selected = GAME_SELECTED.read().unwrap();
                let pack_version = game_selected.pfh_version_by_file_type(PFHFileType::Mod);
                pack_file_decoded = Pack::new_with_name_and_version("unknown.pack", pack_version);
                pack_file_decoded.set_settings(initialize_pack_settings());

                if let Some(version_number) = game_selected.game_version_number(&setting_path(game_selected.game_key_name())) {
                    pack_file_decoded.set_game_version(version_number);
                }
            }

            // In case we want to "Open one or more PackFiles"...
            Command::OpenPackFiles(paths) => {
                match Pack::read_and_merge(&paths, setting_bool("use_lazy_loading"), false) {
                    Ok(pack) => {
                        pack_file_decoded = pack;

                        // Force decoding of table/locs, so they're in memory for the diagnostics to work.
                        if let Some(ref schema) = *SCHEMA.read().unwrap() {
                            let mut decode_extra_data = DecodeableExtraData::default();
                            decode_extra_data.set_schema(Some(schema));
                            let extra_data = Some(decode_extra_data);

                            let mut files = pack_file_decoded.files_by_type_mut(&[FileType::DB, FileType::Loc]);
                            files.par_iter_mut().for_each(|file| {
                                let _ = file.decode(&extra_data, true, false);
                            });
                        }

                        CentralCommand::send_back(&sender, Response::ContainerInfo(ContainerInfo::from(&pack_file_decoded)));
                    }
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                }
            }

            // In case we want to "Open an Extra PackFile" (for "Add from PackFile")...
            Command::OpenPackExtra(path) => {
                match pack_files_decoded_extra.get(&path) {
                    Some(pack) => CentralCommand::send_back(&sender, Response::ContainerInfo(ContainerInfo::from(pack))),
                    None => match Pack::read_and_merge(&[path.to_path_buf()], true, false) {
                         Ok(pack) => {
                            CentralCommand::send_back(&sender, Response::ContainerInfo(ContainerInfo::from(&pack)));
                            pack_files_decoded_extra.insert(path.to_path_buf(), pack);
                        }
                        Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                    }
                }
            }

            // In case we want to "Load All CA PackFiles"...
            Command::LoadAllCAPackFiles => {
                let game_selected = GAME_SELECTED.read().unwrap();
                match Pack::read_and_merge_ca_packs(&game_selected, &setting_path(game_selected.game_key_name())) {
                    Ok(pack) => {
                        pack_file_decoded = pack;

                        // Force decoding of table/locs, so they're in memory for the diagnostics to work.
                        if let Some(ref schema) = *SCHEMA.read().unwrap() {
                            let mut decode_extra_data = DecodeableExtraData::default();
                            decode_extra_data.set_schema(Some(schema));
                            let extra_data = Some(decode_extra_data);

                            let mut files = pack_file_decoded.files_by_type_mut(&[FileType::DB, FileType::Loc]);
                            files.par_iter_mut().for_each(|file| {
                                let _ = file.decode(&extra_data, true, false);
                            });
                        }

                        CentralCommand::send_back(&sender, Response::ContainerInfo(ContainerInfo::from(&pack_file_decoded)));
                    }
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                }
            }

            // In case we want to "Save a PackFile"...
            Command::SavePackFile => {
                let game_selected = GAME_SELECTED.read().unwrap();
                let extra_data = Some(initialize_encodeable_extra_data(&game_selected));

                let pack_type = *pack_file_decoded.header().pfh_file_type();
                if !setting_bool("allow_editing_of_ca_packfiles") && pack_type != PFHFileType::Mod && pack_type != PFHFileType::Movie {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("Pack cannot be saved due to being of CA-Only type. Either change the Pack Type or enable \"Allow Edition of CA Packs\" in the settings.")));
                    continue;
                }

                match pack_file_decoded.save(None, &game_selected, &extra_data) {
                    Ok(_) => CentralCommand::send_back(&sender, Response::ContainerInfo(From::from(&pack_file_decoded))),
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(anyhow!("Error while trying to save the currently open PackFile: {}", error))),
                }
            }

            // In case we want to "Save a PackFile As"...
            Command::SavePackFileAs(path) => {
                let game_selected = GAME_SELECTED.read().unwrap();
                let extra_data = Some(initialize_encodeable_extra_data(&game_selected));

                let pack_type = *pack_file_decoded.header().pfh_file_type();
                if !setting_bool("allow_editing_of_ca_packfiles") && pack_type != PFHFileType::Mod && pack_type != PFHFileType::Movie {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("Pack cannot be saved due to being of CA-Only type. Either change the Pack Type or enable \"Allow Edition of CA Packs\" in the settings.")));
                    continue;
                }

                match pack_file_decoded.save(Some(&path), &game_selected, &extra_data) {
                    Ok(_) => CentralCommand::send_back(&sender, Response::ContainerInfo(From::from(&pack_file_decoded))),
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(anyhow!("Error while trying to save the currently open PackFile: {}", error))),
                }
            }

            // If you want to perform a clean&save over a PackFile...
            Command::CleanAndSavePackFileAs(path) => {
                pack_file_decoded.clean_undecoded();

                let game_selected = GAME_SELECTED.read().unwrap();
                let extra_data = Some(initialize_encodeable_extra_data(&game_selected));
                match pack_file_decoded.save(Some(&path), &game_selected, &extra_data) {
                    Ok(_) => CentralCommand::send_back(&sender, Response::ContainerInfo(From::from(&pack_file_decoded))),
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(anyhow!("Error while trying to save the currently open PackFile: {}", error))),
                }
            }

            // In case we want to get the data of a PackFile needed to form the TreeView...
            Command::GetPackFileDataForTreeView => {

                // Get the name and the PackedFile list, and send it.
                CentralCommand::send_back(&sender, Response::ContainerInfoVecRFileInfo((
                    From::from(&pack_file_decoded),
                    pack_file_decoded.files().par_iter().map(|(_, file)| From::from(file)).collect(),

                )));
            }

            // In case we want to get the data of a Secondary PackFile needed to form the TreeView...
            Command::GetPackFileExtraDataForTreeView(path) => {

                // Get the name and the PackedFile list, and serialize it.
                match pack_files_decoded_extra.get(&path) {
                    Some(pack_file) => CentralCommand::send_back(&sender, Response::ContainerInfoVecRFileInfo((
                        From::from(pack_file),
                        pack_file.files().par_iter().map(|(_, file)| From::from(file)).collect(),
                    ))),
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("Cannot find extra PackFile with path: {}", path.to_string_lossy()))),
                }
            }

            // In case we want to get the info of one PackedFile from the TreeView.
            Command::GetRFileInfo(path) => {
                CentralCommand::send_back(&sender, Response::OptionRFileInfo(
                    pack_file_decoded.files().get(&path).map(From::from)
                ));
            }

            // In case we want to get the info of more than one PackedFiles from the TreeView.
            Command::GetPackedFilesInfo(paths) => {
                let paths = paths.iter().map(|path| ContainerPath::File(path.to_owned())).collect::<Vec<_>>();
                CentralCommand::send_back(&sender, Response::VecRFileInfo(
                    pack_file_decoded.files_by_paths(&paths, false).into_iter().map(From::from).collect()
                ));
            }

            // In case we want to launch a global search on a `PackFile`...
            Command::GlobalSearch(mut global_search) => {
                let game_selected = GAME_SELECTED.read().unwrap();
                match *SCHEMA.read().unwrap() {
                    Some(ref schema) => {
                        global_search.search(&game_selected, schema, &mut pack_file_decoded, &mut dependencies.write().unwrap(), &[]);
                        let packed_files_info = RFileInfo::info_from_global_search(&global_search, &pack_file_decoded);
                        CentralCommand::send_back(&sender, Response::GlobalSearchVecRFileInfo(global_search, packed_files_info));
                    }
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("Schema not found. Maybe you need to download it?"))),
                }
            }

            Command::SetGameSelected(game_selected, rebuild_dependencies) => {
                info!("Setting game selected.");
                let game_changed = GAME_SELECTED.read().unwrap().game_key_name() != game_selected || !FIRST_GAME_CHANGE_DONE.load(Ordering::SeqCst);
                *GAME_SELECTED.write().unwrap() = SUPPORTED_GAMES.game(&game_selected).unwrap();
                let game = GAME_SELECTED.read().unwrap();

                // Optimisation: If we know we need to rebuild the whole dependencies, load them in another thread
                // while we load the schema. That way we can speed-up the entire game-switching process.
                //
                // While this is fast, the rust compiler doesn't like the fact that we're moving out the dependencies,
                // then moving them back in an if, so we need two branches of code, depending on if rebuild is true or not.
                //
                // Branch 1: dependencies rebuilt.
                if rebuild_dependencies {
                info!("Branch 1.");
                    let pack_dependencies = pack_file_decoded.dependencies().to_vec();
                    let handle = thread::spawn(move || {
                        let game_selected = GAME_SELECTED.read().unwrap();
                        let game_path = setting_path(game_selected.game_key_name());
                        let file_path = dependencies_cache_path().unwrap().join(game_selected.dependencies_cache_file_name());
                        let file_path = if game_changed { Some(&*file_path) } else { None };
                        let _ = dependencies.write().unwrap().rebuild(&None, &pack_dependencies, file_path, &game_selected, &game_path);
                        dependencies
                    });

                    // Load the new schemas.
                    load_schemas(&sender, &mut pack_file_decoded, &game);

                    // Get the dependencies that were loading in parallel and send their info to the UI.
                    dependencies = handle.join().unwrap();
                    let dependencies_info = DependenciesInfo::from(&*dependencies.read().unwrap());
                    info!("Sending dependencies info after game selected change.");
                    CentralCommand::send_back(&sender, Response::DependenciesInfo(dependencies_info));

                    // Decode the dependencies tables while the UI does its own thing.
                    dependencies.write().unwrap().decode_tables(&SCHEMA.read().unwrap());
                }

                // Branch 2: no dependecies rebuild.
                else {
                info!("Branch 2.");

                    // Load the new schemas.
                    load_schemas(&sender, &mut pack_file_decoded, &game);
                };

                // If there is a Pack open, change his id to match the one of the new `Game Selected`.
                if !pack_file_decoded.disk_file_path().is_empty() {
                    let pfh_file_type = *pack_file_decoded.header().pfh_file_type();
                    pack_file_decoded.header_mut().set_pfh_version(game.pfh_version_by_file_type(pfh_file_type));

                    if let Some(version_number) = game.game_version_number(&setting_path(game.game_key_name())) {
                        pack_file_decoded.set_game_version(version_number);
                    }
                }
                info!("Switching game selected done.");
            }

            // In case we want to generate the dependencies cache for our Game Selected...
            Command::GenerateDependenciesCache => {
                let game_selected = GAME_SELECTED.read().unwrap();
                let game_path = setting_path(game_selected.game_key_name());
                let asskit_path = assembly_kit_path().ok();

                if game_path.is_dir() {
                    match Dependencies::generate_dependencies_cache(&game_selected, &game_path, &asskit_path) {
                        Ok(mut cache) => {
                            let dependencies_path = dependencies_cache_path().unwrap().join(game_selected.dependencies_cache_file_name());
                            match cache.save(&dependencies_path) {
                                Ok(_) => {
                                    let _ = dependencies.write().unwrap().rebuild(&SCHEMA.read().unwrap(), pack_file_decoded.dependencies(), Some(&dependencies_path), &game_selected, &game_path);
                                    let dependencies_info = DependenciesInfo::from(&*dependencies.read().unwrap());
                                    CentralCommand::send_back(&sender, Response::DependenciesInfo(dependencies_info));
                                },
                                Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                            }
                        }
                        Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                    }
                } else {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("Game Path not configured. Go to <i>'PackFile/Preferences'</i> and configure it.")));
                }
            }

            // In case we want to update the Schema for our Game Selected...
            Command::UpdateCurrentSchemaFromAssKit => {
                if let Some(ref mut schema) = *SCHEMA.write().unwrap() {
                    let game_selected = GAME_SELECTED.read().unwrap();
                    let asskit_path = setting_path(&format!("{}_assembly_kit", game_selected.game_key_name()));
                    let schema_path = schemas_path().unwrap().join(game_selected.schema_file_name());

                    let dependencies = dependencies.read().unwrap();
                    if let Ok(tables_to_check) = dependencies.db_and_loc_data(true, false, true, false) {

                        // Split the tables to check by table name.
                        let mut tables_to_check_split: HashMap<String, Vec<DB>> = HashMap::new();
                        for table_to_check in tables_to_check {
                            if let Ok(RFileDecoded::DB(table)) = table_to_check.decoded() {
                                match tables_to_check_split.get_mut(table.table_name()) {
                                    Some(tables) => {
                                        tables.push(table.clone());
                                    }
                                    None => {
                                        tables_to_check_split.insert(table.table_name().to_owned(), vec![table.clone()]);
                                    }
                                }
                            }
                        }

                        let tables_to_skip = dependencies.vanilla_tables().keys().map(|x| &**x).collect::<Vec<_>>();
                        match update_schema_from_raw_files(schema, &game_selected, &asskit_path, &schema_path, &tables_to_skip, &tables_to_check_split) {
                            Ok(_) => {

                                if dependencies.bruteforce_loc_key_order(schema).is_ok() {
                                    match schema.save(&schemas_path().unwrap().join(GAME_SELECTED.read().unwrap().schema_file_name())) {
                                        Ok(_) => CentralCommand::send_back(&sender, Response::Success),
                                        Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                                    }
                                } else {
                                    CentralCommand::send_back(&sender, Response::Success)
                                }
                            },
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    }
                } else {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("There is no Schema for the Game Selected.")));
                }
            }

            // In case we want to optimize our PackFile...
            Command::OptimizePackFile => {
                if let Some(ref schema) = *SCHEMA.read().unwrap() {
                    match pack_file_decoded.optimize(&mut dependencies.write().unwrap(), schema, setting_bool("optimize_not_renamed_packedfiles")) {
                        Ok(paths_to_delete) => CentralCommand::send_back(&sender, Response::HashSetString(paths_to_delete)),
                        Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                    }
                } else {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("There is no Schema for the Game Selected.")));
                }
            }

            // In case we want to Patch the SiegeAI of a PackFile...
            Command::PatchSiegeAI => {
                match pack_file_decoded.patch_siege_ai() {
                    Ok(result) => CentralCommand::send_back(&sender, Response::StringVecContainerPath(result.0, result.1)),
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error)))
                }
            }

            // In case we want to change the PackFile's Type...
            Command::SetPackFileType(new_type) => pack_file_decoded.set_pfh_file_type(new_type),

            // In case we want to change the "Include Last Modified Date" setting of the PackFile...
            Command::ChangeIndexIncludesTimestamp(state) => {
                let mut bitmask = pack_file_decoded.bitmask();
                bitmask.set(PFHFlags::HAS_INDEX_WITH_TIMESTAMPS, state);
                pack_file_decoded.set_bitmask(bitmask);
            },

            // In case we want to compress/decompress the PackedFiles of the currently open PackFile...
            Command::ChangeDataIsCompressed(state) => { pack_file_decoded.set_compress(state); },

            // In case we want to get the path of the currently open `PackFile`.
            Command::GetPackFilePath => CentralCommand::send_back(&sender, Response::PathBuf(PathBuf::from(pack_file_decoded.disk_file_path()))),

            // In case we want to get the Dependency PackFiles of our PackFile...
            Command::GetDependencyPackFilesList => CentralCommand::send_back(&sender, Response::VecString(pack_file_decoded.dependencies().to_vec())),

            // In case we want to set the Dependency PackFiles of our PackFile...
            Command::SetDependencyPackFilesList(packs) => { pack_file_decoded.set_dependencies(packs); },

            // In case we want to check if there is a Dependency Database loaded...
            Command::IsThereADependencyDatabase(include_asskit) => {
                let are_dependencies_loaded = dependencies.read().unwrap().is_vanilla_data_loaded(include_asskit);
                CentralCommand::send_back(&sender, Response::Bool(are_dependencies_loaded))
            },

            // In case we want to create a PackedFile from scratch...
            Command::NewPackedFile(path, new_packed_file) => {
                let decoded = match new_packed_file {
                    NewFile::AnimPack(_) => {
                        let file = AnimPack::default();
                        RFileDecoded::AnimPack(file)
                    },
                    NewFile::DB(_, table, version) => {
                        if let Some(ref schema) = *SCHEMA.read().unwrap() {
                            match schema.definition_by_name_and_version(&table, version) {
                                Some(definition) => {
                                    let patches = schema.patches_for_table(&table);
                                    let file = DB::new(definition, patches, &table, false);
                                    RFileDecoded::DB(file)
                                }
                                None => {
                                    CentralCommand::send_back(&sender, Response::Error(anyhow!("No definitions found for the table `{}`, version `{}` in the currently loaded schema.", table, version)));
                                    continue;
                                }
                            }
                        } else {
                            CentralCommand::send_back(&sender, Response::Error(anyhow!("There is no Schema for the Game Selected.")));
                            continue;
                        }
                    },
                    NewFile::Loc(_) => {
                        let file = Loc::new(false);
                        RFileDecoded::Loc(file)
                    }
                    NewFile::PortraitSettings(_, version, entries) => {
                        let mut file = PortraitSettings::default();
                        file.set_version(version);

                        if !entries.is_empty() {

                            let mut dependencies = dependencies.write().unwrap();
                            let mut vanilla_files = dependencies.files_by_types_mut(&[FileType::PortraitSettings], true, true);
                            let vanilla_files_decoded = vanilla_files.iter_mut()
                                .filter_map(|(_, file)| file.decode(&None, false, true).ok().flatten())
                                .filter_map(|file| if let RFileDecoded::PortraitSettings(file) = file { Some(file) } else { None })
                                .collect::<Vec<_>>();

                            let vanilla_values = vanilla_files_decoded.iter()
                                .flat_map(|file| file.entries())
                                .map(|entry| (entry.id(), entry))
                                .collect::<HashMap<_,_>>();

                            for (from_id, to_id) in entries {
                                if let Some(from_entry) = vanilla_values.get(&from_id) {
                                    let mut new_entry = (*from_entry).clone();
                                    new_entry.set_id(to_id);
                                    file.entries_mut().push(new_entry);
                                }
                            }
                        }

                        RFileDecoded::PortraitSettings(file)
                    },
                    NewFile::Text(_, text_type) => {
                        let mut file = Text::default();
                        file.set_format(text_type);
                        RFileDecoded::Text(file)
                    },
                };
                let file = RFile::new_from_decoded(&decoded, 0, &path);
                match pack_file_decoded.insert(file) {
                    Ok(_) => CentralCommand::send_back(&sender, Response::Success),
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                }
            }

            // When we want to add one or more PackedFiles to our PackFile.
            Command::AddPackedFiles(source_paths, destination_paths, paths_to_ignore) => {
                let mut added_paths = vec![];
                let mut it_broke = None;

                let paths = source_paths.iter().zip(destination_paths.iter()).collect::<Vec<(&PathBuf, &ContainerPath)>>();
                let schema = SCHEMA.read().unwrap();
                for (source_path, destination_path) in paths {

                    // Skip ignored paths.
                    if let Some(ref paths_to_ignore) = paths_to_ignore {
                        if paths_to_ignore.iter().any(|x| source_path.starts_with(x)) {
                            continue;
                        }
                    }

                    match destination_path {
                        ContainerPath::File(destination_path) => {
                            match pack_file_decoded.insert_file(source_path, destination_path, &schema) {
                                Ok(path) => if let Some(path) = path {
                                    added_paths.push(path);
                                },
                                Err(error) => it_broke = Some(error),
                            }
                        },

                        // TODO: See what should we do with the ignored paths.
                        ContainerPath::Folder(destination_path) => {
                            match pack_file_decoded.insert_folder(source_path, destination_path, &None, &schema, setting_bool("include_base_folder_on_add_from_folder")) {
                                Ok(mut paths) => added_paths.append(&mut paths),
                                Err(error) => it_broke = Some(error),
                            }
                        },
                    }
                }

                if let Some(error) = it_broke {
                    CentralCommand::send_back(&sender, Response::VecContainerPath(added_paths.to_vec()));
                    CentralCommand::send_back(&sender, Response::Error(From::from(error)));
                } else {
                    CentralCommand::send_back(&sender, Response::VecContainerPath(added_paths.to_vec()));
                    CentralCommand::send_back(&sender, Response::Success);
                }

                // Force decoding of table/locs, so they're in memory for the diagnostics to work.
                if let Some(ref schema) = *SCHEMA.read().unwrap() {
                    let mut decode_extra_data = DecodeableExtraData::default();
                    decode_extra_data.set_schema(Some(schema));
                    let extra_data = Some(decode_extra_data);

                    let mut files = pack_file_decoded.files_by_paths_mut(&added_paths, false);
                    files.par_iter_mut().for_each(|file| {
                        let _ = file.decode(&extra_data, true, false);
                    });
                }
            }

            // In case we want to move stuff from one PackFile to another...
            Command::AddPackedFilesFromPackFile((pack_file_path, paths)) => {
                match pack_files_decoded_extra.get(&pack_file_path) {

                    // Try to add the PackedFile to the main PackFile.
                    Some(pack) => {
                        let files = pack.files_by_paths(&paths, false);
                        for file in files {
                            let _ = pack_file_decoded.insert(file.clone());
                        }

                        CentralCommand::send_back(&sender, Response::VecContainerPath(paths.to_vec()));

                        // Force decoding of table/locs, so they're in memory for the diagnostics to work.
                        if let Some(ref schema) = *SCHEMA.read().unwrap() {
                            let mut decode_extra_data = DecodeableExtraData::default();
                            decode_extra_data.set_schema(Some(schema));
                            let extra_data = Some(decode_extra_data);

                            let mut files = pack_file_decoded.files_by_type_mut(&[FileType::DB, FileType::Loc]);
                            files.par_iter_mut().for_each(|file| {
                                let _ = file.decode(&extra_data, true, false);
                            });
                        }
                    }
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("Cannot find extra PackFile with path: {}", pack_file_path.to_string_lossy()))),
                }
            }

            // In case we want to move stuff from our PackFile to an Animpack...
            Command::AddPackedFilesFromPackFileToAnimpack(anim_pack_path, paths) => {
                let files = pack_file_decoded.files_by_paths(&paths, false).into_iter().cloned().collect::<Vec<RFile>>();
                match pack_file_decoded.files_mut().get_mut(&anim_pack_path) {
                    Some(file) => {

                        // Try to decode it using lazy_load if enabled.
                        let extra_data = DecodeableExtraData::default();
                        //extra_data.set_lazy_load(setting_bool("use_lazy_loading"));
                        let _ = file.decode(&Some(extra_data), true, false);

                        match file.decoded_mut() {
                            Ok(decoded) => match decoded {
                                RFileDecoded::AnimPack(anim_pack) => {
                                    for file in files {
                                        let _ = anim_pack.insert(file);
                                    }

                                    CentralCommand::send_back(&sender, Response::VecContainerPath(paths.to_vec()));
                                }
                                _ => CentralCommand::send_back(&sender, Response::Error(anyhow!("We expected {} to be of type {} but found {}. This is either a bug or you did weird things with the game selected.", anim_pack_path, FileType::AnimPack, FileType::from(&*decoded)))),
                            }
                            _ => CentralCommand::send_back(&sender, Response::Error(anyhow!("Failed to decode the file at the following path: {}", anim_pack_path))),
                        }
                    }
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("File not found in the Pack: {}.", anim_pack_path))),
                }
            }

            // In case we want to move stuff from an Animpack to our PackFile...
            Command::AddPackedFilesFromAnimpack(data_source, anim_pack_path, paths) => {
                let mut dependencies = dependencies.write().unwrap();
                let anim_pack_file = match data_source {
                    DataSource::PackFile => pack_file_decoded.files_mut().get_mut(&anim_pack_path),
                    DataSource::GameFiles => dependencies.file_mut(&anim_pack_path, true, false).ok(),
                    DataSource::ParentFiles => dependencies.file_mut(&anim_pack_path, false, true).ok(),
                    DataSource::AssKitFiles |
                    DataSource::ExternalFile => unreachable!("add_files_to_animpack"),
                };

                let files = match anim_pack_file {
                    Some(file) => {

                        // Try to decode it using lazy_load if enabled.
                        let extra_data = DecodeableExtraData::default();
                        //extra_data.set_lazy_load(setting_bool("use_lazy_loading"));
                        let _ = file.decode(&Some(extra_data), true, false);

                        match file.decoded_mut() {
                            Ok(decoded) => match decoded {
                                RFileDecoded::AnimPack(anim_pack) => anim_pack.files_by_paths(&paths, false).into_iter().cloned().collect::<Vec<RFile>>(),
                                _ => {
                                    CentralCommand::send_back(&sender, Response::Error(anyhow!("We expected {} to be of type {} but found {}. This is either a bug or you did weird things with the game selected.", anim_pack_path, FileType::AnimPack, FileType::from(&*decoded))));
                                    continue;
                                },
                            }
                            _ => {
                                CentralCommand::send_back(&sender, Response::Error(anyhow!("Failed to decode the file at the following path: {}", anim_pack_path)));
                                continue;
                            },
                        }
                    }
                    None => {
                        CentralCommand::send_back(&sender, Response::Error(anyhow!("The file with the path {} doesn't exists on the open Pack.", anim_pack_path)));
                        continue;
                    }
                };

                let paths = files.iter().map(|file| file.path_in_container()).collect::<Vec<_>>();
                for mut file in files {
                    let _ = file.guess_file_type();
                    let _ = pack_file_decoded.insert(file);
                }

                CentralCommand::send_back(&sender, Response::VecContainerPath(paths));
            }

            // In case we want to delete files from an Animpack...
            Command::DeleteFromAnimpack((anim_pack_path, paths)) => {
                match pack_file_decoded.files_mut().get_mut(&anim_pack_path) {
                    Some(file) => {

                        // Try to decode it using lazy_load if enabled.
                        let extra_data = DecodeableExtraData::default();
                        //extra_data.set_lazy_load(setting_bool("use_lazy_loading"));
                        let _ = file.decode(&Some(extra_data), true, false);

                        match file.decoded_mut() {
                            Ok(decoded) => match decoded {
                                RFileDecoded::AnimPack(anim_pack) => {
                                    for path in paths {
                                        anim_pack.remove(&path);
                                    }

                                    CentralCommand::send_back(&sender, Response::Success);
                                }
                                _ => CentralCommand::send_back(&sender, Response::Error(anyhow!("We expected {} to be of type {} but found {}. This is either a bug or you did weird things with the game selected.", anim_pack_path, FileType::AnimPack, FileType::from(&*decoded)))),
                            }
                            _ => CentralCommand::send_back(&sender, Response::Error(anyhow!("Failed to decode the file at the following path: {}", anim_pack_path))),
                        }
                    }
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("File not found in the Pack: {}.", anim_pack_path))),
                }
            }

            // In case we want to decode a RigidModel PackedFile...
            Command::DecodePackedFile(path, data_source) => {
                dbg!(&path);
                dbg!(&data_source);
                match data_source {
                    DataSource::PackFile => {
                        if path == RESERVED_NAME_NOTES {
                            let mut note = Text::default();
                            note.set_format(TextFormat::Markdown);
                            note.set_contents(pack_file_decoded.notes().pack_notes().to_owned());
                            CentralCommand::send_back(&sender, Response::Text(note));
                        }

                        else {

                            // Find the PackedFile we want and send back the response.
                            match pack_file_decoded.files_mut().get_mut(&path) {
                                Some(file) => {
                                    dbg!(file.file_type());
                                    let mut extra_data = DecodeableExtraData::default();
                                    //extra_data.set_lazy_load(setting_bool("use_lazy_loading"));

                                    let schema = SCHEMA.read().unwrap();
                                    extra_data.set_schema(schema.as_ref());

                                    let game_key = GAME_SELECTED.read().unwrap().game_key_name();
                                    extra_data.set_game_key(Some(game_key));

                                    let result = file.decode(&Some(extra_data), true, true).transpose().unwrap();

                                    match result {
                                        Ok(RFileDecoded::AnimFragment(data)) => CentralCommand::send_back(&sender, Response::AnimFragmentRFileInfo(data, From::from(&*file))),
                                        Ok(RFileDecoded::AnimPack(data)) => CentralCommand::send_back(&sender, Response::AnimPackRFileInfo(data.files().values().map(From::from).collect(), From::from(&*file))),
                                        Ok(RFileDecoded::AnimsTable(data)) => CentralCommand::send_back(&sender, Response::AnimsTableRFileInfo(data, From::from(&*file))),
                                        Ok(RFileDecoded::Audio(data)) => CentralCommand::send_back(&sender, Response::AudioRFileInfo(data, From::from(&*file))),
                                        Ok(RFileDecoded::ESF(data)) => CentralCommand::send_back(&sender, Response::ESFRFileInfo(data, From::from(&*file))),
                                        Ok(RFileDecoded::DB(table)) => CentralCommand::send_back(&sender, Response::DBRFileInfo(table, From::from(&*file))),
                                        Ok(RFileDecoded::Image(image)) => CentralCommand::send_back(&sender, Response::ImageRFileInfo(image, From::from(&*file))),
                                        Ok(RFileDecoded::Loc(table)) => CentralCommand::send_back(&sender, Response::LocRFileInfo(table, From::from(&*file))),
                                        Ok(RFileDecoded::MatchedCombat(data)) => CentralCommand::send_back(&sender, Response::MatchedCombatRFileInfo(data, From::from(&*file))),
                                        Ok(RFileDecoded::PortraitSettings(data)) => CentralCommand::send_back(&sender, Response::PortraitSettingsRFileInfo(data, From::from(&*file))),
                                        #[cfg(feature = "support_rigidmodel")]Ok(RFileDecoded::RigidModel(rigid_model)) => CentralCommand::send_back(&sender, Response::RigidModelRFileInfo(rigid_model, From::from(&*file))),
                                        Ok(RFileDecoded::Text(text)) => CentralCommand::send_back(&sender, Response::TextRFileInfo(text, From::from(&*file))),
                                        Ok(RFileDecoded::UIC(uic)) => CentralCommand::send_back(&sender, Response::UICRFileInfo(uic, From::from(&*file))),
                                        Ok(RFileDecoded::UnitVariant(_)) => CentralCommand::send_back(&sender, Response::RFileDecodedRFileInfo(result.unwrap(), From::from(&*file))),
                                        Ok(RFileDecoded::Video(data)) => CentralCommand::send_back(&sender, Response::VideoInfoRFileInfo(From::from(&data), From::from(&*file))),
                                        Ok(_) => CentralCommand::send_back(&sender, Response::Unknown),
                                        Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                                    }
                                }
                                None => CentralCommand::send_back(&sender, Response::Error(anyhow!("The file with the path {} hasn't been found on this Pack.", path))),
                            }
                        }
                    }

                    DataSource::ParentFiles => {
                        match dependencies.write().unwrap().file_mut(&path, false, true) {
                            Ok(file) => {
                                let mut extra_data = DecodeableExtraData::default();
                                //extra_data.set_lazy_load(setting_bool("use_lazy_loading"));

                                let schema = SCHEMA.read().unwrap();
                                extra_data.set_schema(schema.as_ref());

                                let game_key = GAME_SELECTED.read().unwrap().game_key_name();
                                extra_data.set_game_key(Some(game_key));

                                let result = file.decode(&Some(extra_data), true, true).transpose().unwrap();

                                match result {
                                    Ok(RFileDecoded::AnimFragment(data)) => CentralCommand::send_back(&sender, Response::AnimFragmentRFileInfo(data, From::from(&*file))),
                                    Ok(RFileDecoded::AnimPack(data)) => CentralCommand::send_back(&sender, Response::AnimPackRFileInfo(data.files().values().map(From::from).collect(), From::from(&*file))),
                                    Ok(RFileDecoded::AnimsTable(data)) => CentralCommand::send_back(&sender, Response::AnimsTableRFileInfo(data, From::from(&*file))),
                                    Ok(RFileDecoded::Audio(data)) => CentralCommand::send_back(&sender, Response::AudioRFileInfo(data, From::from(&*file))),
                                    Ok(RFileDecoded::ESF(data)) => CentralCommand::send_back(&sender, Response::ESFRFileInfo(data, From::from(&*file))),
                                    Ok(RFileDecoded::DB(table)) => CentralCommand::send_back(&sender, Response::DBRFileInfo(table, From::from(&*file))),
                                    Ok(RFileDecoded::Image(image)) => CentralCommand::send_back(&sender, Response::ImageRFileInfo(image, From::from(&*file))),
                                    Ok(RFileDecoded::Loc(table)) => CentralCommand::send_back(&sender, Response::LocRFileInfo(table, From::from(&*file))),
                                    Ok(RFileDecoded::MatchedCombat(data)) => CentralCommand::send_back(&sender, Response::MatchedCombatRFileInfo(data, From::from(&*file))),
                                    Ok(RFileDecoded::PortraitSettings(data)) => CentralCommand::send_back(&sender, Response::PortraitSettingsRFileInfo(data, From::from(&*file))),
                                    #[cfg(feature = "support_rigidmodel")]Ok(RFileDecoded::RigidModel(rigid_model)) => CentralCommand::send_back(&sender, Response::RigidModelRFileInfo(rigid_model, From::from(&*file))),
                                    Ok(RFileDecoded::Text(text)) => CentralCommand::send_back(&sender, Response::TextRFileInfo(text, From::from(&*file))),
                                    Ok(RFileDecoded::UIC(uic)) => CentralCommand::send_back(&sender, Response::UICRFileInfo(uic, From::from(&*file))),
                                    Ok(RFileDecoded::UnitVariant(_)) => CentralCommand::send_back(&sender, Response::RFileDecodedRFileInfo(result.unwrap(), From::from(&*file))),
                                    Ok(RFileDecoded::Video(data)) => CentralCommand::send_back(&sender, Response::VideoInfoRFileInfo(From::from(&data), From::from(&*file))),
                                    Ok(_) => CentralCommand::send_back(&sender, Response::Unknown),
                                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                                }
                            }
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    }

                    DataSource::GameFiles => {
                        match dependencies.write().unwrap().file_mut(&path, true, false) {
                            Ok(file) => {
                                dbg!(file.file_type());
                                let mut extra_data = DecodeableExtraData::default();
                                //extra_data.set_lazy_load(setting_bool("use_lazy_loading"));

                                let schema = SCHEMA.read().unwrap();
                                extra_data.set_schema(schema.as_ref());

                                let game_key = GAME_SELECTED.read().unwrap().game_key_name();
                                extra_data.set_game_key(Some(game_key));

                                let result = file.decode(&Some(extra_data), true, true).transpose().unwrap();

                                match result {
                                    Ok(RFileDecoded::AnimFragment(data)) => CentralCommand::send_back(&sender, Response::AnimFragmentRFileInfo(data, From::from(&*file))),
                                    Ok(RFileDecoded::AnimPack(data)) => CentralCommand::send_back(&sender, Response::AnimPackRFileInfo(data.files().values().map(From::from).collect(), From::from(&*file))),
                                    Ok(RFileDecoded::AnimsTable(data)) => CentralCommand::send_back(&sender, Response::AnimsTableRFileInfo(data, From::from(&*file))),
                                    Ok(RFileDecoded::Audio(data)) => CentralCommand::send_back(&sender, Response::AudioRFileInfo(data, From::from(&*file))),
                                    Ok(RFileDecoded::ESF(data)) => CentralCommand::send_back(&sender, Response::ESFRFileInfo(data, From::from(&*file))),
                                    Ok(RFileDecoded::DB(table)) => CentralCommand::send_back(&sender, Response::DBRFileInfo(table, From::from(&*file))),
                                    Ok(RFileDecoded::Image(image)) => CentralCommand::send_back(&sender, Response::ImageRFileInfo(image, From::from(&*file))),
                                    Ok(RFileDecoded::Loc(table)) => CentralCommand::send_back(&sender, Response::LocRFileInfo(table, From::from(&*file))),
                                    Ok(RFileDecoded::MatchedCombat(data)) => CentralCommand::send_back(&sender, Response::MatchedCombatRFileInfo(data, From::from(&*file))),
                                    Ok(RFileDecoded::PortraitSettings(data)) => CentralCommand::send_back(&sender, Response::PortraitSettingsRFileInfo(data, From::from(&*file))),
                                    #[cfg(feature = "support_rigidmodel")]Ok(RFileDecoded::RigidModel(rigid_model)) => CentralCommand::send_back(&sender, Response::RigidModelRFileInfo(rigid_model, From::from(&*file))),
                                    Ok(RFileDecoded::Text(text)) => CentralCommand::send_back(&sender, Response::TextRFileInfo(text, From::from(&*file))),
                                    Ok(RFileDecoded::UIC(uic)) => CentralCommand::send_back(&sender, Response::UICRFileInfo(uic, From::from(&*file))),
                                    Ok(RFileDecoded::UnitVariant(_)) => CentralCommand::send_back(&sender, Response::RFileDecodedRFileInfo(result.unwrap(), From::from(&*file))),
                                    Ok(RFileDecoded::Video(data)) => CentralCommand::send_back(&sender, Response::VideoInfoRFileInfo(From::from(&data), From::from(&*file))),
                                    Ok(_) => CentralCommand::send_back(&sender, Response::Unknown),
                                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                                }
                            }
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    }

                    DataSource::AssKitFiles => {
                        let path_split = path.split('/').collect::<Vec<_>>();
                        if path_split.len() > 2 {
                            match dependencies.read().unwrap().asskit_only_db_tables().get(path_split[1]) {
                                Some(db) => CentralCommand::send_back(&sender, Response::DBRFileInfo(db.clone(), RFileInfo::default())),
                                None => CentralCommand::send_back(&sender, Response::Error(anyhow!("Table {} not found on Assembly Kit files.", path))),
                            }
                        } else {
                            CentralCommand::send_back(&sender, Response::Error(anyhow!("Path {} doesn't contain an identificable table name.", path)));
                        }
                    }

                    DataSource::ExternalFile => {}
                }
            }

            // When we want to save a PackedFile from the view....
            Command::SavePackedFileFromView(path, file_decoded) => {
                if path == RESERVED_NAME_NOTES {
                    if let RFileDecoded::Text(data) = file_decoded {
                        pack_file_decoded.notes_mut().set_pack_notes(data.contents().to_owned());
                    }
                }
                else if let Some(file) = pack_file_decoded.files_mut().get_mut(&path) {
                    if let Err(error) = file.set_decoded(file_decoded) {
                        CentralCommand::send_back(&sender, Response::Error(From::from(error)));
                    }
                }
                CentralCommand::send_back(&sender, Response::Success);
            }

            // In case we want to delete PackedFiles from a PackFile...
            Command::DeletePackedFiles(paths) => CentralCommand::send_back(&sender, Response::VecContainerPath(paths.iter().flat_map(|path| pack_file_decoded.remove(path)).collect())),

            // In case we want to extract PackedFiles from a PackFile...
            Command::ExtractPackedFiles(container_paths, path, extract_tables_to_tsv) => {
                let schema = SCHEMA.read().unwrap();
                let schema = if extract_tables_to_tsv { &*schema } else { &None };
                let mut errors = 0;

                let extra_data = Some(initialize_encodeable_extra_data(&GAME_SELECTED.read().unwrap()));
                let mut extracted_paths = vec![];

                // Pack extraction.
                if let Some(container_paths) = container_paths.get(&DataSource::PackFile) {
                    for container_path in container_paths {
                        match pack_file_decoded.extract(container_path.clone(), &path, true, schema, false, &extra_data) {
                            Ok(mut extracted_path) => extracted_paths.append(&mut extracted_path),
                            Err(_) => errors += 1,
                        }
                    }

                    if errors == 0 {
                        CentralCommand::send_back(&sender, Response::StringVecPathBuf(tr("files_extracted_success"), extracted_paths));
                    } else {
                        CentralCommand::send_back(&sender, Response::Error(anyhow!("There were {} errors while extracting.", errors)));
                    }
                }

                // Dependencies extraction.
                else {

                    let dependencies = dependencies.read().unwrap();
                    let mut game_files = if let Some(container_paths) = container_paths.get(&DataSource::GameFiles) {
                        dependencies.files_by_path(container_paths, true, false, false)
                    } else {
                        HashMap::new()
                    };
                    let parent_files = if let Some(container_paths) = container_paths.get(&DataSource::ParentFiles) {
                        dependencies.files_by_path(container_paths, false, true, false)
                    } else {
                        HashMap::new()
                    };

                    game_files.extend(parent_files);

                    let mut pack = Pack::default();
                    for (path_raw, file) in game_files {
                        if pack.insert(file.clone()).is_err() {
                            errors += 1;
                            continue;
                        }

                        let container_path = ContainerPath::File(path_raw);
                        match pack.extract(container_path, &path, true, schema, false, &extra_data) {
                            Ok(mut extracted_path) => extracted_paths.append(&mut extracted_path),
                            Err(_) => errors += 1,
                        }
                    }

                    if errors == 0 {
                        CentralCommand::send_back(&sender, Response::StringVecPathBuf(tr("files_extracted_success"), extracted_paths));
                    } else {
                        CentralCommand::send_back(&sender, Response::Error(anyhow!("There were {} errors while extracting.", errors)));
                    }
                }
            }

            // In case we want to rename one or more files/folders...
            Command::RenamePackedFiles(renaming_data) => {
                match pack_file_decoded.move_paths(&renaming_data) {
                    Ok(data) => CentralCommand::send_back(&sender, Response::VecContainerPathContainerPath(data)),
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                }
            }

            // In case we want to know if a Folder exists, knowing his path...
            Command::FolderExists(path) => {
                CentralCommand::send_back(&sender, Response::Bool(pack_file_decoded.has_folder(&path)));
            }

            // In case we want to know if PackedFile exists, knowing his path...
            Command::PackedFileExists(path) => {
                CentralCommand::send_back(&sender, Response::Bool(pack_file_decoded.has_file(&path)));
            }

            // In case we want to get the list of tables in the dependency database...
            Command::GetTableListFromDependencyPackFile => CentralCommand::send_back(&sender, Response::VecString(dependencies.read().unwrap().vanilla_tables().keys().map(|x| x.to_owned()).collect())),

            Command::LocalArtSetIds => CentralCommand::send_back(&sender, Response::HashSetString(dependencies.read().unwrap().db_values_from_table_name_and_column_name(Some(&pack_file_decoded), "campaign_character_arts_tables", "art_set_id", false, false))),

            // TODO: This needs to use a list pulled from portrait settings files, not from a table.
            Command::DependenciesArtSetIds => CentralCommand::send_back(&sender, Response::HashSetString(dependencies.read().unwrap().db_values_from_table_name_and_column_name(None, "campaign_character_arts_tables", "art_set_id", true, true))),

            // In case we want to get the version of an specific table from the dependency database...
            Command::GetTableVersionFromDependencyPackFile(table_name) => {
                if dependencies.read().unwrap().is_vanilla_data_loaded(false) {
                    match dependencies.read().unwrap().db_version(&table_name) {
                        Some(version) => CentralCommand::send_back(&sender, Response::I32(version)),
                        None => CentralCommand::send_back(&sender, Response::Error(anyhow!("Table not found in the game files."))),
                    }
                } else { CentralCommand::send_back(&sender, Response::Error(anyhow!("Dependencies cache needs to be regenerated before this."))); }
            }

            #[cfg(feature = "enable_tools")] Command::GetTableDefinitionFromDependencyPackFile(table_name) => {
                if dependencies.read().unwrap().is_vanilla_data_loaded(false) {
                    if let Some(ref schema) = *SCHEMA.read().unwrap() {
                        if let Some(version) = dependencies.read().unwrap().db_version(&table_name) {
                            if let Some(definition) = schema.definition_by_name_and_version(&table_name, version) {
                                CentralCommand::send_back(&sender, Response::Definition(definition.clone()));
                            } else { CentralCommand::send_back(&sender, Response::Error(anyhow!("No definition found for table {}.", table_name))); }
                        } else { CentralCommand::send_back(&sender, Response::Error(anyhow!("Table version not found in dependencies for table {}.", table_name))); }
                    } else { CentralCommand::send_back(&sender, Response::Error(anyhow!("There is no Schema for the Game Selected."))); }
                } else { CentralCommand::send_back(&sender, Response::Error(anyhow!("Dependencies cache needs to be regenerated before this."))); }
            }

            // In case we want to merge DB or Loc Tables from a PackFile...
            Command::MergeFiles(paths, merged_path, delete_source_files) => {
                let files_to_merge = pack_file_decoded.files_by_paths(&paths, false);
                match RFile::merge(&files_to_merge, &merged_path) {
                    Ok(file) => {
                        let _ = pack_file_decoded.insert(file);

                        if delete_source_files {
                            paths.iter().for_each(|path| { pack_file_decoded.remove(path); });
                        }

                        CentralCommand::send_back(&sender, Response::String(merged_path.to_string()));
                    },
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                }
            }

            // In case we want to update a table...
            Command::UpdateTable(path) => {
                let path = path.path_raw();
                if let Some(rfile) = pack_file_decoded.file_mut(path) {
                    if let Ok(decoded) = rfile.decoded_mut() {
                        match dependencies.write().unwrap().update_db(decoded) {
                            Ok((old_version, new_version)) => CentralCommand::send_back(&sender, Response::I32I32(old_version, new_version)),
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    } else { CentralCommand::send_back(&sender, Response::Error(anyhow!("File with the following path undecoded: {}", path))); }
                } else { CentralCommand::send_back(&sender, Response::Error(anyhow!("File not found in the open Pack: {}", path))); }
            }

            // In case we want to replace all matches in a Global Search...
            Command::GlobalSearchReplaceMatches(mut global_search, matches) => {
                let game_info = GAME_SELECTED.read().unwrap();
                if let Some(ref schema) = *SCHEMA.read().unwrap() {
                    let paths = global_search.replace(&game_info, schema, &mut pack_file_decoded, &mut dependencies.write().unwrap(), &matches);
                    let files_info = paths.iter().flat_map(|path| pack_file_decoded.files_by_path(path, false).iter().map(|file| RFileInfo::from(*file)).collect::<Vec<RFileInfo>>()).collect();

                    CentralCommand::send_back(&sender, Response::GlobalSearchVecRFileInfo(global_search, files_info));
                } else {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("Schema not found. Maybe you need to download it?")));
                }
            }

            // In case we want to replace all matches in a Global Search...
            Command::GlobalSearchReplaceAll(mut global_search) => {
                let game_info = GAME_SELECTED.read().unwrap();
                if let Some(ref schema) = *SCHEMA.read().unwrap() {
                    let paths = global_search.replace_all(&game_info, schema, &mut pack_file_decoded, &mut dependencies.write().unwrap());
                    let files_info = paths.iter().flat_map(|path| pack_file_decoded.files_by_path(path, false).iter().map(|file| RFileInfo::from(*file)).collect::<Vec<RFileInfo>>()).collect();

                    CentralCommand::send_back(&sender, Response::GlobalSearchVecRFileInfo(global_search, files_info));
                } else {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("Schema not found. Maybe you need to download it?")));
                }
            }

            // In case we want to get the reference data for a definition...
            Command::GetReferenceDataFromDefinition(table_name, definition) => {

                // TODO: move this to pack opening.
                dependencies.write().unwrap().generate_local_definition_references(&table_name, &definition);
                let reference_data = dependencies.read().unwrap().db_reference_data(&pack_file_decoded, &table_name, &definition);
                CentralCommand::send_back(&sender, Response::HashMapI32TableReferences(reference_data));
            }

            // In case we want to change the format of a ca_vp8 video...
            Command::SetVideoFormat(path, format) => {
                match pack_file_decoded.files_mut().get_mut(&path) {
                    Some(ref mut rfile) => {
                        match rfile.decoded_mut() {
                            Ok(data) => {
                                if let RFileDecoded::Video(ref mut data) = data {
                                    data.set_format(format);
                                }
                                // TODO: Put an error here.
                            }
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    }
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("This Pack doesn't exists as a file in the disk."))),
                }
            },

            // In case we want to save an schema to disk...
            Command::SaveSchema(mut schema) => {
                match schema.save(&schemas_path().unwrap().join(GAME_SELECTED.read().unwrap().schema_file_name())) {
                    Ok(_) => {
                        *SCHEMA.write().unwrap() = Some(schema);
                        CentralCommand::send_back(&sender, Response::Success);
                    },
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                }
            }

            // In case we want to clean the cache of one or more PackedFiles...
            Command::CleanCache(paths) => {
                let mut files = pack_file_decoded.files_by_paths_mut(&paths, false);
                let extra_data = Some(initialize_encodeable_extra_data(&GAME_SELECTED.read().unwrap()));

                files.iter_mut().for_each(|file| {
                    let _ = file.encode(&extra_data, true, true, false);
                });
            }

            // In case we want to export a PackedFile as a TSV file...
            Command::ExportTSV(internal_path, external_path) => {
                let schema = SCHEMA.read().unwrap();
                match &*schema {
                    Some(ref schema) => {
                        match pack_file_decoded.file_mut(&internal_path) {
                            Some(file) => match file.tsv_export_to_path(&external_path, schema) {
                                Ok(_) => CentralCommand::send_back(&sender, Response::Success),
                                Err(error) =>  CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                            }
                            None => CentralCommand::send_back(&sender, Response::Error(anyhow!("File with the following path not found in the Pack: {}", internal_path))),
                        }
                    },
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("There is no Schema for the Game Selected."))),
                }
            }

            // In case we want to import a TSV as a PackedFile...
            // TODO: This is... unreliable at best, can break stuff at worst. Replace the set_decoded with proper type checking.
            Command::ImportTSV(internal_path, external_path) => {
                let schema = SCHEMA.read().unwrap();
                match &*schema {
                    Some(ref schema) => {
                        match pack_file_decoded.file_mut(&internal_path) {
                            Some(file) => {
                                match RFile::tsv_import_from_path(&external_path, schema) {
                                    Ok(imported) => {
                                        let decoded = imported.decoded().unwrap();
                                        file.set_decoded(decoded.clone()).unwrap();
                                        CentralCommand::send_back(&sender, Response::RFileDecoded(decoded.clone()))
                                    },
                                    Err(error) =>  CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                                }
                            }
                            None => CentralCommand::send_back(&sender, Response::Error(anyhow!("File with the following path not found in the Pack: {}", internal_path))),
                        }
                    },
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("There is no Schema for the Game Selected."))),
                }
            }

            // In case we want to open a PackFile's location in the file manager...
            Command::OpenContainingFolder => {

                // If the path exists, try to open it. If not, throw an error.
                let mut path = PathBuf::from(pack_file_decoded.disk_file_path());
                if path.exists() {
                    path.pop();
                    let _ = open::that(&path);
                    CentralCommand::send_back(&sender, Response::Success);
                }
                else {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("This Pack doesn't exists as a file in the disk.")));
                }
            },

            // When we want to open a PackedFile in a external program...
            Command::OpenPackedFileInExternalProgram(data_source, path) => {
                match data_source {
                    DataSource::PackFile => {
                        let folder = temp_dir().join(format!("rpfm_{}", pack_file_decoded.disk_file_name()));
                        let extra_data = Some(initialize_encodeable_extra_data(&GAME_SELECTED.read().unwrap()));

                        match pack_file_decoded.extract(path.clone(), &folder, true, &SCHEMA.read().unwrap(), false, &extra_data) {
                            Ok(extracted_path) => {
                                let _ = that(&extracted_path[0]);
                                CentralCommand::send_back(&sender, Response::PathBuf(extracted_path[0].to_owned()));
                            }
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    }
                    _ => todo!("Make cases for dependencies."),
                }
            }

            // When we want to save a PackedFile from the external view....
            Command::SavePackedFileFromExternalView(path, external_path) => {
                match save_files_from_external_path(&mut pack_file_decoded, &path, &external_path) {
                    Ok(_) => CentralCommand::send_back(&sender, Response::Success),
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                }
            }

            // When we want to update our schemas...
            Command::UpdateSchemas => {
                match schemas_path() {
                    Ok(local_path) => {
                        let git_integration = GitIntegration::new(&local_path, SCHEMA_REPO, SCHEMA_BRANCH, SCHEMA_REMOTE);
                        match git_integration.update_repo() {
                            Ok(_) => {
                                let game = GAME_SELECTED.read().unwrap();
                                let schema_path = schemas_path().unwrap().join(game.schema_file_name());

                                // Encode the decoded tables with the old schema, then re-decode them with the new one.
                                let mut tables = pack_file_decoded.files_by_type_mut(&[FileType::DB]);
                                let extra_data = Some(initialize_encodeable_extra_data(&GAME_SELECTED.read().unwrap()));

                                tables.par_iter_mut().for_each(|x| { let _ = x.encode(&extra_data, true, true, false); });

                                *SCHEMA.write().unwrap() = Schema::load(&schema_path).ok();

                                if let Some(ref schema) = *SCHEMA.read().unwrap() {
                                    let mut extra_data = DecodeableExtraData::default();
                                    extra_data.set_schema(Some(schema));
                                    let extra_data = Some(extra_data);

                                    tables.par_iter_mut().for_each(|x| {
                                        let _ = x.decode(&extra_data, true, false);
                                    });

                                    // Then rebuild the dependencies stuff.
                                    if dependencies.read().unwrap().is_vanilla_data_loaded(false) {
                                        let game_path = setting_path(game.game_key_name());
                                        let dependencies_file_path = dependencies_cache_path().unwrap().join(game.dependencies_cache_file_name());

                                        match dependencies.write().unwrap().rebuild(&SCHEMA.read().unwrap(), pack_file_decoded.dependencies(), Some(&*dependencies_file_path), &game, &game_path) {
                                            Ok(_) => CentralCommand::send_back(&sender, Response::Success),
                                            Err(_) => CentralCommand::send_back(&sender, Response::Error(anyhow!("Schema updated, but dependencies cache rebuilding failed. You may need to regenerate it."))),
                                        }
                                    } else {
                                        CentralCommand::send_back(&sender, Response::Success)
                                    }
                                } else {
                                    CentralCommand::send_back(&sender, Response::Success)
                                }
                            },
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    },
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(error)),
                }
            }

            // When we want to update our lua setup...
            Command::UpdateLuaAutogen => {
                match lua_autogen_base_path() {
                    Ok(local_path) => {
                        let git_integration = GitIntegration::new(&local_path, LUA_REPO, LUA_BRANCH, LUA_REMOTE);
                        match git_integration.update_repo() {
                            Ok(_) => CentralCommand::send_back(&sender, Response::Success),
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    },
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(error)),
                }
            }

            // When we want to update our program...
            Command::UpdateMainProgram => {
                match crate::updater::update_main_program() {
                    Ok(_) => CentralCommand::send_back(&sender, Response::Success),
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(error)),
                }
            }

            // When we want to update our program...
            Command::TriggerBackupAutosave => {

                // Note: we no longer notify the UI of success or error to not hang it up.
                let folder = backup_autosave_path().unwrap().join(pack_file_decoded.disk_file_name());
                let _ = DirBuilder::new().recursive(true).create(&folder);
                if folder.is_dir() {
                    let date = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
                    let date_formatted = OffsetDateTime::from_unix_timestamp(date as i64).unwrap().format(&FULL_DATE_FORMAT).unwrap();
                    let new_name = format!("{date_formatted}.pack");
                    let new_path = folder.join(new_name);
                    if pack_file_decoded.pfh_file_type() == PFHFileType::Mod {
                        let game_selected = GAME_SELECTED.read().unwrap();
                        let extra_data = Some(initialize_encodeable_extra_data(&game_selected));

                        let pack_type = *pack_file_decoded.header().pfh_file_type();
                        if !setting_bool("allow_editing_of_ca_packfiles") && pack_type != PFHFileType::Mod && pack_type != PFHFileType::Movie {
                            CentralCommand::send_back(&sender, Response::Error(anyhow!("Pack cannot be saved due to being of CA-Only type. Either change the Pack Type or enable \"Allow Edition of CA Packs\" in the settings.")));
                            continue;
                        }

                        let _ = pack_file_decoded.clone().save(Some(&new_path), &game_selected, &extra_data);
                    }

                    // If we have more than the limit, delete the older one.
                    if let Ok(files) = files_in_folder_from_newest_to_oldest(&folder) {
                        let max_files = setting_int("autosave_amount") as usize;
                        for (index, file) in files.iter().enumerate() {
                            if index >= max_files {
                                let _ = std::fs::remove_file(file);
                            }
                        }
                    }
                }
            }

            // In case we want to perform a diagnostics check...
            Command::DiagnosticsCheck(diagnostics_ignored) => {

                // Spawn a separate thread so the UI can keep working.
                thread::spawn(clone!(
                    dependencies,
                    pack_file_decoded => move || {
                    let game_selected = GAME_SELECTED.read().unwrap().clone();
                    let game_path = setting_path(game_selected.game_key_name());

                    let mut diagnostics = Diagnostics::default();
                    *diagnostics.diagnostics_ignored_mut() = diagnostics_ignored;

                    if let Some(schema) = &*SCHEMA.read().unwrap() {
                        if pack_file_decoded.pfh_file_type() == PFHFileType::Mod ||
                            pack_file_decoded.pfh_file_type() == PFHFileType::Movie {
                            diagnostics.check(&pack_file_decoded, &mut dependencies.write().unwrap(), &game_selected, &game_path, &[], schema);
                        }

                        info!("Checking diagnostics: done.");
                    }

                    CentralCommand::send_back(&sender, Response::Diagnostics(diagnostics));
                }));
            }

            Command::DiagnosticsUpdate(mut diagnostics, path_types) => {

                // Spawn a separate thread so the UI can keep working.
                thread::spawn(clone!(
                    dependencies,
                    pack_file_decoded => move || {
                    let game_selected = GAME_SELECTED.read().unwrap().clone();
                    let game_path = setting_path(game_selected.game_key_name());

                    if let Some(schema) = &*SCHEMA.read().unwrap() {
                        if pack_file_decoded.pfh_file_type() == PFHFileType::Mod ||
                            pack_file_decoded.pfh_file_type() == PFHFileType::Movie {
                            diagnostics.check(&pack_file_decoded, &mut dependencies.write().unwrap(), &game_selected, &game_path, &path_types, schema);
                        }

                        info!("Checking diagnostics (update): done.");
                    }

                    CentralCommand::send_back(&sender, Response::Diagnostics(diagnostics));
                }));
            }

            // In case we want to get the open PackFile's Settings...
            Command::GetPackSettings => CentralCommand::send_back(&sender, Response::PackSettings(pack_file_decoded.settings().clone())),
            Command::SetPackSettings(settings) => { pack_file_decoded.set_settings(settings); }

            Command::GetMissingDefinitions => {

                // Test to see if every DB Table can be decoded. This is slow and only useful when
                // a new patch lands and you want to know what tables you need to decode.
                let mut counter = 0;
                let mut table_list = String::new();
                if let Some(ref schema) = *SCHEMA.read().unwrap() {
                    let mut extra_data = DecodeableExtraData::default();
                    extra_data.set_schema(Some(schema));
                    let extra_data = Some(extra_data);

                    let mut files = pack_file_decoded.files_by_type_mut(&[FileType::DB]);
                    files.sort_by_key(|file| file.path_in_container_raw().to_lowercase());

                    for file in files {
                        if file.decode(&extra_data, false, false).is_err() && file.load().is_ok() {
                            if let Ok(raw_data) = file.cached() {
                                let mut reader = Cursor::new(raw_data);
                                if let Ok((_, _, _, entry_count)) = DB::read_header(&mut reader) {
                                    if entry_count > 0 {
                                        counter += 1;
                                        table_list.push_str(&format!("{}, {:?}\n", counter, file.path_in_container_raw()))
                                    }
                                }
                            }
                        }
                    }
                }

                // Try to save the file. And I mean "try". Someone seems to love crashing here...
                let path = PROGRAM_PATH.to_path_buf().join(PathBuf::from("missing_table_definitions.txt"));

                if let Ok(file) = File::create(path) {
                    let mut file = BufWriter::new(file);
                    let _ = file.write_all(table_list.as_bytes());
                }
            }

            // Ignore errors for now.
            Command::RebuildDependencies(rebuild_only_current_mod_dependencies) => {
                if SCHEMA.read().unwrap().is_some() {
                    let game_selected = GAME_SELECTED.read().unwrap();
                    let game_path = setting_path(game_selected.game_key_name());
                    let dependencies_file_path = dependencies_cache_path().unwrap().join(game_selected.dependencies_cache_file_name());
                    let file_path = if !rebuild_only_current_mod_dependencies { Some(&*dependencies_file_path) } else { None };

                    let _ = dependencies.write().unwrap().rebuild(&SCHEMA.read().unwrap(), pack_file_decoded.dependencies(), file_path, &game_selected, &game_path);
                    let dependencies_info = DependenciesInfo::from(&*dependencies.read().unwrap());
                    CentralCommand::send_back(&sender, Response::DependenciesInfo(dependencies_info));
                } else {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("There is no Schema for the Game Selected.")));
                }
            },

            Command::CascadeEdition(table_name, definition, changes) => {
                let edited_paths = changes.iter().flat_map(|(field, value_before, value_after)| {
                    DB::cascade_edition(&mut pack_file_decoded, &SCHEMA.read().unwrap(), &table_name, field, &definition, value_before, value_after)
                }).collect::<Vec<_>>();

                let packed_files_info = pack_file_decoded.files_by_paths(&edited_paths, false).into_par_iter().map(From::from).collect();
                CentralCommand::send_back(&sender, Response::VecContainerPathVecRFileInfo(edited_paths, packed_files_info));
            }

            Command::GoToDefinition(ref_table, ref_column, ref_data) => {
                let table_name = format!("{ref_table}_tables");
                let table_folder = format!("db/{table_name}");
                let packed_files = pack_file_decoded.files_by_path(&ContainerPath::Folder(table_folder.to_owned()), true);
                let mut found = false;
                for packed_file in &packed_files {
                    if let Ok(RFileDecoded::DB(data)) = packed_file.decoded() {
                        if let Some((column_index, row_index)) = data.table().rows_containing_data(&ref_column, &ref_data) {
                            CentralCommand::send_back(&sender, Response::DataSourceStringUsizeUsize(DataSource::PackFile, packed_file.path_in_container_raw().to_owned(), column_index, row_index[0]));
                            found = true;
                            break;
                        }
                    }
                }

                if !found {
                    if let Ok(packed_files) = dependencies.read().unwrap().db_data(&table_name, false, true) {
                        for packed_file in &packed_files {
                            if let Ok(RFileDecoded::DB(data)) = packed_file.decoded() {
                                if let Some((column_index, row_index)) = data.table().rows_containing_data(&ref_column, &ref_data) {
                                    CentralCommand::send_back(&sender, Response::DataSourceStringUsizeUsize(DataSource::ParentFiles, packed_file.path_in_container_raw().to_owned(), column_index, row_index[0]));
                                    found = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                if !found {
                    if let Ok(packed_files) = dependencies.read().unwrap().db_data(&table_name, true, false) {
                        for packed_file in &packed_files {
                            if let Ok(RFileDecoded::DB(data)) = packed_file.decoded() {
                                if let Some((column_index, row_index)) = data.table().rows_containing_data(&ref_column, &ref_data) {
                                    CentralCommand::send_back(&sender, Response::DataSourceStringUsizeUsize(DataSource::GameFiles, packed_file.path_in_container_raw().to_owned(), column_index, row_index[0]));
                                    found = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                if !found {
                    if let Some(table) = dependencies.read().unwrap().asskit_only_db_tables().get(&table_name) {
                        if let Some((column_index, row_index)) = table.table().rows_containing_data(&ref_column, &ref_data) {
                            let path = format!("{}/ak_data", &table_folder);
                            CentralCommand::send_back(&sender, Response::DataSourceStringUsizeUsize(DataSource::AssKitFiles, path, column_index, row_index[0]));
                            found = true;
                        }
                    }
                }

                if !found {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!(tr("source_data_for_field_not_found"))));
                }
            },

            Command::SearchReferences(reference_map, value) => {
                let paths = reference_map.keys().map(|x| ContainerPath::Folder(format!("db/{x}"))).collect::<Vec<ContainerPath>>();
                let files = pack_file_decoded.files_by_paths(&paths, true);

                let mut references: Vec<(DataSource, String, String, usize, usize)> = vec![];

                // Pass for local tables.
                for (table_name, columns) in &reference_map {
                    for file in &files {
                        if file.db_table_name_from_path().unwrap() == table_name {
                            if let Ok(RFileDecoded::DB(data)) = file.decoded() {
                                for column_name in columns {
                                    if let Some((column_index, row_indexes)) = data.table().rows_containing_data(column_name, &value) {
                                        for row_index in &row_indexes {
                                            references.push((DataSource::PackFile, file.path_in_container_raw().to_owned(), column_name.to_owned(), column_index, *row_index));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Pass for parent tables.
                for (table_name, columns) in &reference_map {
                        if let Ok(tables) = dependencies.read().unwrap().db_data(table_name, false, true) {
                        references.append(&mut tables.par_iter().map(|table| {
                            let mut references = vec![];
                            if let Ok(RFileDecoded::DB(data)) = table.decoded() {
                                for column_name in columns {
                                    if let Some((column_index, row_indexes)) = data.table().rows_containing_data(column_name, &value) {
                                        for row_index in &row_indexes {
                                            references.push((DataSource::ParentFiles, table.path_in_container_raw().to_owned(), column_name.to_owned(), column_index, *row_index));
                                        }
                                    }
                                }
                            }

                            references
                        }).flatten().collect());
                    }
                }

                // Pass for vanilla tables.
                for (table_name, columns) in &reference_map {
                    if let Ok(tables) = dependencies.read().unwrap().db_data(table_name, true, false) {
                        references.append(&mut tables.par_iter().map(|table| {
                            let mut references = vec![];
                            if let Ok(RFileDecoded::DB(data)) = table.decoded() {
                                for column_name in columns {
                                    if let Some((column_index, row_indexes)) = data.table().rows_containing_data(column_name, &value) {
                                        for row_index in &row_indexes {
                                            references.push((DataSource::GameFiles, table.path_in_container_raw().to_owned(), column_name.to_owned(), column_index, *row_index));
                                        }
                                    }
                                }
                            }

                            references
                        }).flatten().collect());
                    }
                }

                CentralCommand::send_back(&sender, Response::VecDataSourceStringStringUsizeUsize(references));
            },

            Command::GoToLoc(loc_key) => {
                let packed_files = pack_file_decoded.files_by_type(&[FileType::Loc]);
                let mut found = false;
                for packed_file in &packed_files {
                    if let Ok(RFileDecoded::Loc(data)) = packed_file.decoded() {
                        if let Some((column_index, row_index)) = data.table().rows_containing_data("key", &loc_key) {
                            CentralCommand::send_back(&sender, Response::DataSourceStringUsizeUsize(DataSource::PackFile, packed_file.path_in_container_raw().to_owned(), column_index, row_index[0]));
                            found = true;
                            break;
                        }
                    }
                }

                if !found {
                    if let Ok(packed_files) = dependencies.read().unwrap().loc_data(false, true) {
                        for packed_file in &packed_files {
                            if let Ok(RFileDecoded::Loc(data)) = packed_file.decoded() {
                                if let Some((column_index, row_index)) = data.table().rows_containing_data("key", &loc_key) {
                                    CentralCommand::send_back(&sender, Response::DataSourceStringUsizeUsize(DataSource::ParentFiles, packed_file.path_in_container_raw().to_owned(), column_index, row_index[0]));
                                    found = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                if !found {
                    if let Ok(packed_files) = dependencies.read().unwrap().loc_data(true, false) {
                        for packed_file in &packed_files {
                            if let Ok(RFileDecoded::Loc(data)) = packed_file.decoded() {
                                if let Some((column_index, row_index)) = data.table().rows_containing_data("key", &loc_key) {
                                    CentralCommand::send_back(&sender, Response::DataSourceStringUsizeUsize(DataSource::GameFiles, packed_file.path_in_container_raw().to_owned(), column_index, row_index[0]));
                                    found = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                if !found {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!(tr("loc_key_not_found"))));
                }
            },

            Command::GetSourceDataFromLocKey(loc_key) => CentralCommand::send_back(&sender, Response::OptionStringStringString(dependencies.read().unwrap().loc_key_source(&loc_key))),
            Command::GetPackFileName => CentralCommand::send_back(&sender, Response::String(pack_file_decoded.disk_file_name())),
            Command::GetPackedFileRawData(path) => {
                match pack_file_decoded.files_mut().get_mut(&path) {
                    Some(ref mut rfile) => {

                        // Make sure it's in memory.
                        match rfile.load() {
                            Ok(_) => match rfile.cached() {
                                Ok(data) => CentralCommand::send_back(&sender, Response::VecU8(data.to_vec())),

                                // If we don't have binary data, it may be decoded. Encode it and return the binary data.
                                Err(_) =>  {
                                    let extra_data = Some(initialize_encodeable_extra_data(&GAME_SELECTED.read().unwrap()));
                                    match rfile.encode(&extra_data, false, false, true) {
                                        Ok(data) => CentralCommand::send_back(&sender, Response::VecU8(data.unwrap())),
                                        Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                                    }
                                },
                            },
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    }
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("This PackedFile no longer exists in the PackFile."))),
                }
            },

            Command::ImportDependenciesToOpenPackFile(paths_by_data_source) => {
                let mut added_paths = vec![];

                let dependencies = dependencies.read().unwrap();
                for (data_source, paths) in &paths_by_data_source {
                    let files = match data_source {
                        DataSource::GameFiles => dependencies.files_by_path(paths, true, false, false),
                        DataSource::ParentFiles => dependencies.files_by_path(paths, false, true, false),

                        _ => {
                            CentralCommand::send_back(&sender, Response::Error(anyhow!("You can't import files from this source.")));
                            CentralCommand::send_back(&sender, Response::Success);
                            continue 'background_loop;
                        },
                    };

                    for file in files.into_values() {
                        let mut file = file.clone();
                        let _ = file.guess_file_type();
                        if let Ok(Some(path)) = pack_file_decoded.insert(file) {
                            added_paths.push(path);
                        }
                    }
                }

                CentralCommand::send_back(&sender, Response::VecContainerPath(added_paths));
                CentralCommand::send_back(&sender, Response::Success);
            },

            Command::GetRFilesFromAllSources(paths) => {
                let mut packed_files = HashMap::new();

                // Get PackedFiles requested from the Parent Files.
                let mut packed_files_parent = HashMap::new();
                for (path, file) in dependencies.read().unwrap().files_by_path(&paths, false, true, true) {
                    packed_files_parent.insert(path, file.clone());
                }

                // Get PackedFiles requested from the Game Files.
                let mut packed_files_game = HashMap::new();
                for (path, file) in dependencies.read().unwrap().files_by_path(&paths, true, false, true) {
                    packed_files_game.insert(path, file.clone());
                }

                // Get PackedFiles requested from the AssKit Files.
                //let mut packed_files_asskit = HashMap::new();
                //if let Ok((packed_files_decoded, _)) = dependencies.get_packedfile_from_asskit_files(&paths) {
                //    for packed_file in packed_files_decoded {
                //        packed_files_asskit.insert(packed_file.get_path().to_vec(), packed_file);
                //    }
                //    packed_files.insert(DataSource::AssKitFiles, packed_files_asskit);
                //}

                // Get PackedFiles requested from the currently open PackFile, if any.
                let mut packed_files_packfile = HashMap::new();
                for file in pack_file_decoded.files_by_paths(&paths, true) {
                    packed_files_packfile.insert(file.path_in_container_raw().to_owned(), file.clone());
                }

                packed_files.insert(DataSource::ParentFiles, packed_files_parent);
                packed_files.insert(DataSource::GameFiles, packed_files_game);
                packed_files.insert(DataSource::PackFile, packed_files_packfile);

                // Return the full list of PackedFiles requested, split by source.
                CentralCommand::send_back(&sender, Response::HashMapDataSourceHashMapStringRFile(packed_files));
            },

            #[cfg(feature = "enable_tools")] Command::GetPackedFilesNamesStartingWitPathFromAllSources(path) => {
                let mut files: HashMap<DataSource, HashSet<ContainerPath>> = HashMap::new();
                let dependencies = dependencies.read().unwrap();

                let parent_files = dependencies.files_by_path(&[path.clone()], false, true, true);
                if !parent_files.is_empty() {
                    files.insert(DataSource::ParentFiles, parent_files.into_keys().map(ContainerPath::File).collect());
                }

                let game_files = dependencies.files_by_path(&[path.clone()], true, false, true);
                if !game_files.is_empty() {
                    files.insert(DataSource::GameFiles, game_files.into_keys().map(ContainerPath::File).collect());
                }

                let local_files = pack_file_decoded.files_by_path(&path, true);
                if !local_files.is_empty() {
                    files.insert(DataSource::PackFile, local_files.into_iter().map(|file| file.path_in_container()).collect());
                }

                // Return the full list of PackedFile names requested, split by source.
                CentralCommand::send_back(&sender, Response::HashMapDataSourceHashSetContainerPath(files));
            },

            #[cfg(feature = "enable_tools")] Command::SavePackedFilesToPackFileAndClean(files) => {
                let schema = SCHEMA.read().unwrap();
                match &*schema {
                    Some(ref schema) => {

                        // We receive a list of edited PackedFiles. The UI is the one that takes care of editing them to have the data we want where we want.
                        // Also, the UI is responsible for naming them in case they're new. Here we grab them and directly add them into the PackFile.
                        let mut added_paths = vec![];
                        for file in files {
                            if let Ok(Some(path)) = pack_file_decoded.insert(file) {
                                added_paths.push(path);
                            }
                        }

                        // Clean up duplicates from overwrites.
                        added_paths.sort();
                        added_paths.dedup();

                        // Then, optimize the PackFile. This should remove any non-edited rows/files.
                        match pack_file_decoded.optimize(&mut dependencies.write().unwrap(), schema, false) {
                            Ok(paths_to_delete) => CentralCommand::send_back(&sender, Response::VecContainerPathVecContainerPath(added_paths, paths_to_delete.into_iter()
                                .map(ContainerPath::File)
                                .collect())),
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    },
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("There is no Schema for the Game Selected."))),
                }
            },

            Command::NotesForPath(path) => CentralCommand::send_back(&sender, Response::VecNote(pack_file_decoded.notes().notes_by_path(&path))),
            Command::AddNote(note) => CentralCommand::send_back(&sender, Response::Note(pack_file_decoded.notes_mut().add_note(note))),
            Command::DeleteNote(path, id) => pack_file_decoded.notes_mut().delete_note(&path, id),

            Command::UploadSchemaPatch(table_name, patch) => {
                let filename = "definitionpatch.json";
                let data = serde_json::to_string_pretty(&patch).unwrap();
                dbg!(&data);
                match Logger::send_event(&SENTRY_GUARD.read().unwrap(), Level::Info, &format!("Schema patch for game: {}, table: {}", GAME_SELECTED.read().unwrap().display_name(), table_name), Some((filename, data.as_bytes()))) {
                    Ok(_) => CentralCommand::send_back(&sender, Response::Success),
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                }
            }

            Command::ImportSchemaPatch(patch) => {
                match *SCHEMA.write().unwrap() {
                    Some(ref mut schema) => {
                        schema.add_patch(patch);
                        match schema.save(&schemas_path().unwrap().join(GAME_SELECTED.read().unwrap().schema_file_name())) {
                            Ok(_) => CentralCommand::send_back(&sender, Response::Success),
                            Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                        }
                    }
                    None => CentralCommand::send_back(&sender, Response::Error(anyhow!("There is no Schema for the Game Selected."))),
                }
            }

            Command::GenerateMissingLocData => {
                match pack_file_decoded.generate_missing_loc_data() {
                    Ok(path) => CentralCommand::send_back(&sender, Response::OptionContainerPath(path)),
                    Err(error) => CentralCommand::send_back(&sender, Response::Error(From::from(error))),
                }
            }

            // Initialize the folder for a MyMod, including the folder structure it needs.
            Command::InitializeMyModFolder(mod_name, mod_game, sublime_support, vscode_support, git_support)  => {
                let mut mymod_path = setting_path(MYMOD_BASE_PATH);
                if !mymod_path.is_dir() {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("MyMod path is not configured. Configure it in the settings and try again.")));
                    continue;
                }

                mymod_path.push(&mod_game);

                // Just in case the folder doesn't exist, we try to create it.
                if let Err(error) = DirBuilder::new().recursive(true).create(&mymod_path) {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("Error while creating the MyMod's Game folder: {}.", error.to_string())));
                    continue;
                }

                // We need to create another folder inside the game's folder with the name of the new "MyMod", to store extracted files.
                mymod_path.push(&mod_name);
                if let Err(error) = DirBuilder::new().recursive(true).create(&mymod_path) {
                    CentralCommand::send_back(&sender, Response::Error(anyhow!("Error while creating the MyMod's Assets folder: {}.", error.to_string())));
                    continue;
                };

                // Create a repo inside the MyMod's folder.
                if let Some(gitignore) = git_support {
                    let git_integration = GitIntegration::new(&mymod_path, "", "", "");
                    if let Err(error) = git_integration.init() {
                        CentralCommand::send_back(&sender, Response::Error(From::from(error)));
                        continue
                    }

                    if let Err(error) = git_integration.add_gitignore(&gitignore) {
                        CentralCommand::send_back(&sender, Response::Error(From::from(error)));
                        continue
                    }
                }

                // If the tw_autogen supports the game, create the vscode and sublime configs for lua mods.
                if sublime_support || vscode_support {
                    if let Ok(lua_autogen_folder) = lua_autogen_game_path(&GAME_SELECTED.read().unwrap()) {
                        let lua_autogen_folder = lua_autogen_folder.to_string_lossy().to_string().replace('\\', "/");

                        // VSCode support.
                        if vscode_support {
                            let mut vscode_config_path = mymod_path.to_owned();
                            vscode_config_path.push(".vscode");

                            if let Err(error) = DirBuilder::new().recursive(true).create(&vscode_config_path) {
                                CentralCommand::send_back(&sender, Response::Error(anyhow!("Error while creating the VSCode Config folder: {}.", error.to_string())));
                                continue;
                            };

                            let mut vscode_extensions_path_file = vscode_config_path.to_owned();
                            vscode_extensions_path_file.push("extensions.json");
                            if let Ok(file) = File::create(vscode_extensions_path_file) {
                                let mut file = BufWriter::new(file);
                                let _ = file.write_all("
{
    \"recommendations\": [
        \"sumneko.lua\",
        \"formulahendry.code-runner\"
    ],
}".as_bytes());
                            }
                        }

                        // Sublime support.
                        if sublime_support {
                            let mut sublime_config_path = mymod_path.to_owned();
                            sublime_config_path.push(format!("{}.sublime-project", mymod_path.file_name().unwrap().to_string_lossy()));
                            if let Ok(file) = File::create(sublime_config_path) {
                                let mut file = BufWriter::new(file);
                                let _ = file.write_all("
{
    \"folders\":
    [
        {
            \"path\": \".\"
        }
    ]
}".to_string().as_bytes());
                            }
                        }

                        // Generic lua support.
                        let mut luarc_config_path = mymod_path.to_owned();
                        luarc_config_path.push(".luarc.json");

                        if let Ok(file) = File::create(luarc_config_path) {
                            let mut file = BufWriter::new(file);
                            let _ = file.write_all(format!("
{{
    \"workspace.library\": [
        \"{lua_autogen_folder}/global/\",
        \"{lua_autogen_folder}/campaign/\",
        \"{lua_autogen_folder}/frontend/\",
        \"{lua_autogen_folder}/battle/\"
    ],
    \"runtime.version\": \"Lua 5.1\",
    \"completion.autoRequire\": false,
    \"workspace.preloadFileSize\": 1500,
    \"workspace.ignoreSubmodules\": false,
    \"diagnostics.workspaceDelay\": 500,
    \"diagnostics.workspaceRate\": 40,
    \"diagnostics.disable\": [
        \"lowercase-global\",
        \"trailing-space\"
    ],
    \"hint.setType\": true,
    \"workspace.ignoreDir\": [
        \".vscode\",
        \".git\"
    ]
}}").as_bytes());
                        }
                    }
                }

                // Return the name of the MyMod Pack.
                mymod_path.set_extension("pack");
                CentralCommand::send_back(&sender, Response::PathBuf(mymod_path));
            },

            Command::LiveExport => match live_export(&mut pack_file_decoded) {
                Ok(_) => CentralCommand::send_back(&sender, Response::Success),
                Err(error) => CentralCommand::send_back(&sender, Response::Error(error)),
            },

            // These two belong to the network thread, not to this one!!!!
            Command::CheckUpdates | Command::CheckSchemaUpdates | Command::CheckLuaAutogenUpdates => panic!("{THREADS_COMMUNICATION_ERROR}{response:?}"),
        }
    }
}

/// Function to perform a live extraction.
fn live_export(pack: &mut Pack) -> Result<()> {

    // If there are no files, directly return an error.
    if pack.files().is_empty() {
        return Err(anyhow!("No files to export."));
    }

    let extra_data = Some(initialize_encodeable_extra_data(&GAME_SELECTED.read().unwrap()));
    let game_path = setting_path(GAME_SELECTED.read().unwrap().game_key_name());
    let data_path = GAME_SELECTED.read().unwrap().data_path(&game_path)?;

    // We're interested in lua and xml files only, not those entire folders.
    let files = pack.files_by_type_and_paths(&[FileType::Text], &[ContainerPath::Folder("script/".to_string()), ContainerPath::Folder("ui/".to_string())], true)
        .into_iter()
        .cloned()
        .collect::<Vec<RFile>>();

    let mut correlations = HashMap::new();
    for mut file in files.into_iter() {
        let mut path_split = file.path_in_container_split().iter().map(|x| x.to_owned()).collect::<Vec<_>>();
        let mut hasher = DefaultHasher::new();

        // Use time to ensure we never collide with a previous live export.
        std::time::SystemTime::now().hash(&mut hasher);
        let value = hasher.finish();
        let new_name = format!("{}_{}", value, path_split.last().unwrap());

        *path_split.last_mut().unwrap() = &new_name;
        let new_path = path_split.join("/");

        correlations.insert(file.path_in_container_raw().to_owned(), new_path.to_owned());
        file.set_path_in_container_raw(&new_path);

        // To avoid duplicating logic, we insert these files into the pack, extract them, then delete them from the Pack.
        let container_path = file.path_in_container();
        pack.insert(file)?;
        pack.extract(container_path.clone(), &data_path, true, &None, false, &extra_data)?;

        pack.remove(&container_path);
    }

    // This is the file you have to call from lua later on.
    let summary_data_str = correlations.iter().map(|(key, value)| format!("    [\"{key}\"] = \"{value}\",")).join("\n");
    let summary_data_lua = format!("return {{\n{summary_data_str}\n}}");
    let summary_path = game_path.join("lua_path_mappings.txt");
    let mut file = BufWriter::new(File::create(summary_path)?);
    file.write_all(summary_data_lua.as_bytes())?;

    Ok(())
}

/// Function to simplify logic for changing game selected.
fn load_schemas(sender: &Sender<Response>, pack: &mut Pack, game: &GameInfo) {

    // Before loading the schema, make sure we don't have tables with definitions from the current schema.
    let mut files = pack.files_by_type_mut(&[FileType::DB]);
    let extra_data = Some(initialize_encodeable_extra_data(game));

    files.par_iter_mut().for_each(|file| {
        let _ = file.encode(&extra_data, true, true, false);
    });

    // Load the new schema.
    let schema_path = schemas_path().unwrap().join(game.schema_file_name());
    let _ = Schema::update(&schema_path, &PathBuf::from("schemas/patches.ron"), game.game_key_name());         // Quick fix so we can load old schemas. To be removed once 4.0 lands.
    *SCHEMA.write().unwrap() = Schema::load(&schema_path).ok();

    // Redecode all the tables in the open file.
    if let Some(ref schema) = *SCHEMA.read().unwrap() {
        let mut extra_data = DecodeableExtraData::default();
        extra_data.set_schema(Some(schema));
        let extra_data = Some(extra_data);

        files.par_iter_mut().for_each(|file| {
            let _ = file.decode(&extra_data, true, false);
        });
    }

    // Send a response, so the UI continues working while we finish things here.
    info!("Sending success after game selected change.");
    CentralCommand::send_back(sender, Response::Success);
}

/// Function to save files from external paths, so it's easier to use in the big loop.
///
/// NOTE: If TSV is detected and fails to import, this returns an error.
fn save_files_from_external_path(pack: &mut Pack, internal_path: &str, external_path: &Path) -> Result<()> {

    // We do it manually instead of using insert_file because insert_file replaces the file's metadata.
    let mut file = BufReader::new(File::open(external_path)?);
    let mut data = vec![];
    file.read_to_end(&mut data)?;
    match pack.file_mut(&internal_path) {
        Some(file) => {

            // If we're dealing with a TSV, make sure to import it before setting up the data.
            match external_path.extension() {
                Some(extension) => {
                    if extension.to_string_lossy() == "tsv" {
                        if let Some(ref schema) = *SCHEMA.read().unwrap() {
                            let rfile = RFile::tsv_import_from_path(external_path, schema)?;
                            file.set_decoded(rfile.decoded()?.clone())?;
                        } else {
                            file.set_cached(&data);
                        }
                    } else {
                        file.set_cached(&data);
                    }
                }
                None => {
                    file.set_cached(&data);
                }
            }

            // If they're tables, make sure they're left decoded.
            if file.file_type() == FileType::DB || file.file_type() == FileType::Loc {
                if let Some(ref schema) = *SCHEMA.read().unwrap() {
                    let mut extra_data = DecodeableExtraData::default();
                    extra_data.set_schema(Some(schema));
                    let extra_data = Some(extra_data);
                    let _ = file.decode(&extra_data, true, false);
                }
            }

            Ok(())
        }
        None => Err(anyhow!("Failed to find file with path in pack: {}", internal_path)),
    }
}
