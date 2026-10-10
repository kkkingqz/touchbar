# Declarative Button Sets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Вынести встроенные наборы в отдельные TOML-файлы, отделить их логику от UI и позволить нескольким наборам использовать общий код с независимым состоянием.

**Architecture:** Plugin-host строит `RetainedUi` из проверенного описания и состояния отдельного Wasm-экземпляра логики. Supervisor передаёт файлы набора через sealed bundle, сохраняя broker policy и activation. Sessiond использует прежнюю композицию items, дополненную метаданными наборов и включаемыми пользовательскими профилями.

**Tech Stack:** Rust 2024, serde/toml, WIT Component Model, Wasmtime, существующие touchbar-ui, broker и plugin store; новых внешних библиотек не требуется.

**Spec:** `docs/superpowers/specs/2026-10-09-button-sets-design.md` (согласована пользователем 2026-10-10).

## Global Constraints

- «состояние, запросы, подписки и презентации у наборов независимы».
- «изменение его расположения не требуют пересборки Wasm».
- «у каждого набора один Wasm entrypoint и один владелец policy».
- «Стабильные item IDs сохраняются».
- «Старые world `plugin`, `visibility-aware-plugin`, нативные плагины и appearance providers продолжают поддерживаться».
- «Вложенные includes не допускаются»; дубликаты IDs — ошибка.
- «Каждый конфигурационный файл ограничен 256 KiB, include-граф — 64 файлами и общим размером 1 MiB».
- «Activation создаётся host из реального input»; grants и scope не расширяются описанием.
- «При ошибке продолжает работать последний корректный вариант» пользовательской композиции.
- AUR, вложенный checkout `touchbar/`, установка/постоянный запуск сервисов вне работы. Известная UID mapping несовместимость команд не исправляется ослаблением проверок.

## Review Focus

- Отзыв разрешения во время drag: следующие системные операции отклоняются, UI корректно показывает недоступность (задачи 4, 9).
- Перезапуск или смена профиля с незавершённым контактом: поздний Released/Activated не обращается к новому набору (задачи 4, 8).
- Повторяемый список меняется во время взаимодействия: стабильный ключ сохраняет адресата либо контакт отменяется (задачи 3, 10).
- Первый reload с отсутствующим новым include: прежняя композиция работает, создание файла затем замечается (задача 7).
- Идентичный Wasm двух наборов: одинаковые action IDs не смешивают состояния, запросы и подписки (задачи 4, 8).

## Структура файлов и зависимости

- `crates/touchbar-set-config/src/{lib,schema,state,bundle}.rs`: формат, типизированное состояние, bounded bundle; без зависимости на host/UI.
- `crates/touchbar-logic-sdk/src/{lib,engine,controls}.rs`: bindings нового world и функции движка; переиспользует существующие wire schemas и broker imports.
- `crates/touchbar-plugin-host/src/sets/{mod,renderer,bindings,runtime}.rs`: проверка привязок, сборка UI и запуск логики.
- `crates/touchbar-plugin-host/src/runtime.rs`: enum `PluginRuntime::{Legacy,ButtonSet}` и делегирование общей host API; существующий `PluginHost` не переименовывать.
- `crates/touchbar-package/src/sets.rs`: пакетный каталог наборов и преобразование их метаданных в существующие items/profiles/bars.
- `crates/touchbar-profile-config/src/includes.rs`: загрузка фрагментов с источниками для watcher.
- `plugins/*/sets/*.toml`: отдельный файл каждого основного набора, содержащий также его панели.
- Существующие manifest/store/supervisor/sessiond/CLI модули изменяются только в местах загрузки, запуска и каталогов.

Порядок: 1 → 2 → 3 → 4 → 5 → 6; затем 7 и миграция 8 → 9 → 10 → 11; завершение 12. До задачи 6 новый runtime не считается доступным для production. Ни один этап не заканчивает всю миграцию вместо последующих задач.

## Проверки в текущем окружении

В shell выполнения определить функцию; это инструкция для исполнителя, не новый файл проекта:

```sh
tb_cargo() {
    flatpak-spawn --host distrobox enter --name touchbar-build --no-tty -- \
        sh -lc 'cd /home/king/touchbar && exec cargo "$@"' touchbar-plan "$@"
}
```

Ниже `tb_cargo` означает эту функцию. Для host acceptance использовать:

```sh
flatpak-spawn --host /usr/bin/bash -c \
  'export PATH="/home/king/touchbar/run/host-toolchain:$PATH"; exec /home/king/touchbar/scripts/ci-session.sh /home/king/touchbar/scripts/test-packaging.sh'
```

Тесты confinement/cgroups запускать на host, при необходимости компилируя ELF в контейнере. Временная тестовая сессия не означает установку сервиса. Сначала проверять конкретную задачу; полный workspace и кампании — в конце. Коммитить точные файлы задачи, не использовать `git add .`.

---

### Task 1: Схема отдельного набора и состояния

**Files:** Create `crates/touchbar-set-config/Cargo.toml`, `src/lib.rs`, `src/schema.rs`, `src/state.rs`, `tests/schema.rs`; Modify root `Cargo.toml` и `Cargo.lock`.

**Interfaces:** Produces `SetDocument::from_toml(&str) -> anyhow::Result<Self>`, `SetDocument::validate() -> anyhow::Result<()>`; `StateValue::{Bool,Number,Text,List}`, `StateSnapshot(BTreeMap<String, StateValue>)`, `StatePatch`; `LogicDescription` со схемами actions/state. `SetDocument::validate_bindings(&LogicDescription) -> anyhow::Result<()>`; `StateSnapshot::apply(&StatePatch, &LogicDescription) -> anyhow::Result<bool>` применяет patch атомарно и возвращает наличие изменения.

- [x] Написать тесты `browser_file_is_one_set`, `unknown_fields_are_rejected`, `duplicate_widget_ids_are_rejected`, `limits_match_host_budget`, `state_patch_is_atomic`. Assert: пример из spec содержит 3 кнопки; ошибка при неизвестном поле/повторном ID/NaN; невалидный patch оставляет исходное состояние.

  Опорные assertions для этих тестов (переменные создаёт соответствующий fixture):
  ```rust
  assert_eq!(button_count, 3);
  assert!(SetDocument::from_toml(&unknown_field_document).is_err());
  assert!(snapshot.apply(&invalid_patch, &description).is_err());
  assert_eq!(snapshot, before);
  ```
- [x] Выполнить `tb_cargo test -p touchbar-set-config`; до создания реализации ожидается отсутствие crate/API, после появления тестового crate — ошибки соответствующих тестов.
- [x] Реализовать строгую схему версии 1, `logic_config`, selection, layout, panels, theme roles и event bindings. Общие типы event/value и canvas data держать здесь; не импортировать plugin-host, чтобы не образовать цикл зависимостей. Лимиты: 64 items, 256 узлов описания, 512 развёрнутых узлов, depth 32, 16 KiB текста, 512 canvas commands, 2048 points, 256 path segments, 512 fill triangles, 64 animations; effect bounds — существующие touchbar-ui константы. Для миграции разрешить явный `widget_id: u64` наряду со строковым `id`; его дубликаты тоже запрещены.
- [x] Выполнить `tb_cargo test -p touchbar-set-config`; все тесты проходят.
- [x] Коммит: `feat: define declarative button set schema`.

### Task 2: Контракт логики и SDK

**Files:** Modify `wit/plugin.wit`; Create `crates/touchbar-logic-sdk/{Cargo.toml,src/lib.rs,src/engine.rs,src/controls.rs}` и `examples/button-set-logic/{Cargo.toml,src/lib.rs}`; Modify root workspace и lockfile.

**Interfaces:** Новый world `button-set-logic` импортирует прежний `broker`. Exports: `describe() -> LogicDescription`, `initialize(item_id: string, logic_config: string) -> result<StatePatch,string>`, `handle-action(event: ActionEvent) -> result<LogicUpdate,string>`, `handle-visibility-changed(visible: bool)`, `handle-presentation-event(event: PresentationEvent)`, `handle-host-event(event: HostEvent)`, `handle-context-changed(request: RenderRequest)`; четыре последних возвращают `result<LogicUpdate,string>`. Context callback сообщает viewport/theme без системных эффектов, не возвращает дерево UI. `ActionEvent` содержит action, input kind, optional scalar value, contact ID и optional record key. `LogicUpdate` содержит state patch и прежние presentation commands. Типы WIT переводятся в типы задачи 1 без рекурсивных WIT-типов: список состоит из записей скалярных полей. Фасад предоставляет `keyboard.shortcut(&str)`, `media.subscribe()`, `media.command(&MediaCommand)`, `audio.set_volume(u32)`, `audio.toggle_mute()`, `display.set_brightness(&str, u32)`, `power.set_profile(PowerProfile)`; return types — существующие typed submit results с RequestId, PowerProfile — фиксированный enum.

- [x] Написать `logic_description_roundtrip` и `engine_control_requests_are_scoped`: assert roundtrip описания/state; `set_volume(42)` создаёт существующий named command `set-volume` с ограниченным percent; shortcut/media передают прежний wire request. Публичные `audio.toggle_mute()` и остальные aliases используют только нынешние разрешённые операции: отсутствие подходящего scope возвращает ошибку, а не новое unrestricted command permission.
- [x] Выполнить `tb_cargo test -p touchbar-logic-sdk`; ожидается failure до добавления converters/adapters.
- [x] Реализовать world bindings и фасад `EngineApi::{keyboard,media,audio,display,power}`. Функции возвращают существующие RequestId/ошибки, используют существующие capabilities и operation names. Перенести формирование controls requests и bounded parsing в `controls.rs`; разрешённые command IDs/values сохраняются. Пример экспортирует один action `increment` и state `count`, не строит UI.
- [x] Выполнить `tb_cargo test -p touchbar-logic-sdk` и `tb_cargo build --release --target wasm32-wasip2 --manifest-path examples/button-set-logic/Cargo.toml`. Проверить, что прежние bindings/component examples по-прежнему собираются.
- [x] Коммит: `feat: add button set logic contract and engine adapters`.

### Task 3: Декларативная сборка UI и адресация событий

**Files:** Create `crates/touchbar-plugin-host/src/sets/{mod.rs,renderer.rs,bindings.rs}`; Modify host `Cargo.toml`, `src/lib.rs`; Create `tests/set_renderer.rs`.

**Interfaces:** Consumes task 1. Produces `CompiledSet::new(SetDocument, &LogicDescription, HostLimits) -> anyhow::Result<Self>`, `CompiledSet::render(&StateSnapshot, Rect, Appearance, Duration, &PackageAssets) -> anyhow::Result<RetainedUi>`, `CompiledSet::action_for(&InputEvent) -> anyhow::Result<Option<ActionEvent>>`.

- [x] Написать `layout_order_changes_without_logic`, `missing_action_is_load_error`, `state_type_mismatch_is_load_error`, `repeated_keys_survive_reorder`, `removed_repeat_key_cancels_contact`. Assert порядок semantic nodes соответствует файлу; повторный ключ — ошибка; перестановка не переносит Activated на другой record.
- [x] Выполнить `tb_cargo test -p touchbar-plugin-host --test set_renderer`; новые проверки должны падать до renderer.
- [x] Реализовать containers/spaces/buttons/text/symbol/image/indicator/slider/scrubber/panels/list templates/canvas data на существующих Node/RetainedUi. Compile назначает стабильные widget IDs; таблица bindings учитывает keyed record и event kind. Bound properties читаются из state; renderer не вызывает broker/логику. Rebuild меняет UI только при изменении данных/layout/theme/viewport или текущей анимации.
- [x] Выполнить renderer tests и существующие UI/layout tests. Проверить прежние animation/effect/asset ограничения, без нового busy redraw.
- [x] Коммит: `feat: render button sets from declarative layouts`.

### Task 4: Независимый Wasm runtime и безопасная доставка событий

**Files:** Create `crates/touchbar-plugin-host/src/sets/runtime.rs`, `src/runtime.rs`, `tests/set_runtime.rs`; Modify `src/lib.rs`, `src/main.rs`, `src/replay.rs`, `src/replay_gpu.rs`.

**Interfaces:** `ButtonSetHost::from_bytes_with_broker(component: &[u8], set: SetDocument, limits: HostLimits, broker: Option<BrokerClient>) -> anyhow::Result<Self>`. `PluginRuntime::{Legacy(PluginHost),ButtonSet(ButtonSetHost)}` делегирует прежние `items`, `render`, `render_at`, `handle_event`, presentation/visibility handlers, broker pumping, assets и animation metrics; у CLI/live/replay один runtime enum.

- [x] Написать `same_component_separate_stores`, `init_and_describe_cannot_request_effects`, `action_uses_real_activation`, `revocation_during_drag_blocks_write`, `late_input_after_restart_is_rejected`, `visibility_closes_own_resources`. Assert increment A оставляет B.count=0; broker requests в init/describe запрещены; resource IDs/activation не принадлежат другому Store или generation.

  ```rust
  assert_eq!(count_a_after_activation, 1);
  assert_eq!(count_b_after_activation_of_a, 0);
  assert_eq!(accepted_effects_during_initialize, 0);
  assert_eq!(accepted_effects_after_revocation, 0);
  ```
- [x] Выполнить `tb_cargo test -p touchbar-plugin-host --test set_runtime`; ожидать failure до поддержки нового world.
- [x] Реализовать отдельные Store/HostState на набор; describe/initialize используют фазу без effects, lifecycle и host events — прежние разрешённые фазы. Context callback вызывается при изменении viewport/theme в фазе Render без effects и без presentation commands; затем renderer строит UI из состояния. Применять patch атомарно и проверять presentation ownership. Применять activation_for_input, refuel и resource cleanup так же, как Legacy. Никаких фиктивных contacts или activation из layout. Старый PluginHost API остаётся для существующих тестов.
- [x] Выполнить runtime, broker и legacy host tests; headless/replay пример `increment` показывает count=1 только после action. Пока production loader нового runtime не включать.
- [x] Коммит: `feat: host isolated button set logic instances`.

### Task 5: Каталог наборов в package/store и CLI

**Files:** Create `crates/touchbar-package/src/sets.rs`; Modify package `src/lib.rs`, `Cargo.toml`; store `src/lib.rs`, `Cargo.toml`; CLI `src/main.rs` и `Cargo.toml`; Create `crates/touchbar-plugin-store/tests/button_sets.rs`.

**Interfaces:** `RuntimeSpec::ButtonSetLogic { entrypoint, world }`, `SetReference { id, file }`; `ResolvedSetCatalog { documents, items, profiles, bars }`; `resolve_set_catalog(manifest: &PluginManifest, files: &BTreeMap<String, Vec<u8>>) -> anyhow::Result<ResolvedSetCatalog>`. Catalog metadata преобразуется в существующие ItemContribution/AutomaticProfile/PresentationBar без записи синтетических данных обратно в manifest. Selection.priority хранится отдельно для session catalog. InstalledRuntime остаётся Component для sandboxed logic; world отличает новый контракт.

- [x] Написать `set_files_are_locked_artifacts`, `set_items_install_without_legacy_items`, `mixed_items_and_sets_rejected`, `set_id_must_match_reference`, `set_path_escape_rejected`, `archive_roundtrip_keeps_sets`, `layout_only_update_changes_package_not_wasm`. Assert digest файла присутствует; архив/install содержит описание; изменение TOML меняет package digest, digest Wasm сохраняется.

  ```rust
  assert!(inspection.artifacts.contains_key("sets/browser.toml"));
  assert_ne!(package_digest_before, package_digest_after);
  assert_eq!(wasm_digest_before, wasm_digest_after);
  ```
- [x] Выполнить `tb_cargo test -p touchbar-package -p touchbar-plugin-store`; новые тесты падают.
- [x] Реализовать manifest validation и artifact_paths; reuse normalized relative path/no symlink rules. При inspect/install/pack материализовать каталог в памяти; items/profiles и отключённость сохраняют прежнюю модель store. CLI build/check/pack принимает новый world, использует entrypoint и проверяет set bindings headless. Panels объявляют layout владельца и могут ссылаться на другие явно зарегистрированные items своего пакета через существующие bar refs; ссылки на отсутствующий item — ошибка. Панель владельца использует его Store, ссылки на самостоятельные наборы используют собственные Store. Не создавать скрытые синтетические наборы или второй экземпляр логики только для раскрытия панели.
- [x] Выполнить package/store tests и CLI scaffold/build/check tests для обоих world; старые manifests не требуют нового поля.
- [x] Коммит: `feat: package and validate declarative button sets`.

### Task 6: Sealed transport и включение runtime в sessiond

**Files:** Create set-config `src/bundle.rs`; Modify supervisor `src/main.rs`, sessiond `src/plugins.rs`, host `src/main.rs`, соответствующие Cargo.toml; Create `scripts/test-supervised-button-sets.sh`; Modify `scripts/test-packaging.sh`.

**Interfaces:** `SetBundle::encode(&BTreeMap<String, Vec<u8>>) -> anyhow::Result<Vec<u8>>`, `SetBundle::decode(&[u8]) -> anyhow::Result<Self>` — bounded, no duplicate/unexpected paths. `seal_verified_sets(root, manifest, expected_digests) -> anyhow::Result<OwnedFd>` передаёт полный каталог. Reserved child FD 9, env `TOUCHBAR_SET_BUNDLE_FD`; temporary duplicates остаются >=10. Launch Component дополняется set digests и world, влияющими на Desired equality/restart.

- [x] Написать `set_digest_mismatch_rejected`, `bundle_members_exactly_match_manifest`, `production_uses_sealed_set_bytes`, `missing_bundle_rejected`, `unexpected_bundle_on_legacy_rejected`, `legacy_fds_unchanged`. Assert замена файла после sealing не меняет показанный набор; до sealing выявляется digest mismatch.
- [x] Выполнить unit tests bundle/supervisor и новый host acceptance до реализации; ожидается failure при новом runtime.
- [x] Реализовать sealing с O_NOFOLLOW, file/size/digest проверкой и существующими memfd seals; exact manifest membership. Не перечитывать paths в supervised host. Session manager использует catalog tasks 5 для presentation, selection и launch; policy RuntimeKind остаётся Component. CLI unsupervised/headless читает пакет явно и применяет те же проверки схемы; unsolicited inherited FD запрещён.
- [x] Выполнить focused tests и `test-supervised-button-sets.sh` через host ci-session; закрытие/ошибка всех descriptor branches не даёт FD leaks. Добавить acceptance в packaging campaign.
- [x] Коммит: `feat: launch verified button sets through supervisor`.

### Task 7: Отдельные пользовательские профили и atomic reload

**Files:** Create profile-config `src/includes.rs`; Modify `src/lib.rs`; sessiond `src/profile_watch.rs`, `src/main.rs`; Modify `config/profiles-gnome.toml.example`, Create `config/profiles-gnome/{default,browser,office,media}.toml.example`. В root example include содержит имена файлов после рекомендованного копирования без `.example`.

**Interfaces:** `LoadedProfiles { document: ProfileDocument, sources: BTreeSet<PathBuf> }`; `load_with_sources(path: &Path) -> anyhow::Result<LoadedProfiles>`; существующий `load(path) -> Result<ProfileDocument>` совместимый wrapper. Fragment version 1 содержит profiles/contributions/rules/regions, без fallback и includes. Root `include` указывает список относительных фрагментов. Watcher охватывает root, старые источники и новые запрошенные источники неудачной попытки.

- [x] Написать `legacy_document_unchanged`, `includes_have_explicit_order`, `nested_include_rejected`, `duplicate_id_rejected`, `include_byte_budgets_exact`, `unsafe_owner_mode_link_rejected`, `missing_include_creation_reloads`, `failed_reload_keeps_last_good_catalog`. Assert max 64 файлов вместе с root, суммарно <=1 MiB; каждый <=256 KiB; ошибка не меняет активную композицию.

  ```rust
  assert!(load_with_sources(&root_with_65_files).is_err());
  assert!(load_with_sources(&root_with_nested_include).is_err());
  assert_eq!(active_profile_after_failed_reload, active_profile_before);
  assert!(missing_fragment_creation_noticed);
  ```
- [x] Выполнить `tb_cargo test -p touchbar-profile-config` и watcher tests; missing include recovery первоначально падает.
- [x] Реализовать secure file loading и объединение до общего validate/build. Для path resolution пройти каждый родитель без symlinks, не полагаться только на O_NOFOLLOW конечного файла. Сохранить отсутствие nested includes. Watcher наблюдает nearest existing parents и atomic replacement; конфигурация/каталог меняются только после полного успеха. Explicit user rules остаются выше package selection; одинаковые package priorities имеют стабильный tie-break.
- [x] Выполнить focused tests и `scripts/test-profile-control-live.sh` в тестовой host session; GNOME пример выбирает прежние наборы.
- [x] Коммит: `feat: load separate profile files with atomic reload`.

### Task 8: App Actions и доказательство повторного использования

**Files:** Create `plugins/app-actions/sets/{browser,formatting}.toml`, `src/actions.rs`, `tests/replay.json`; Modify `src/lib.rs`, `touchbar-plugin.toml`, `Cargo.toml`; Add acceptance fixtures второго compact-browser набора к тесту task 6.

**Interfaces:** LogicDescription actions `back`, `forward`, `reload`, `bold`, `italic`, `underline`; state `shortcuts_available: bool`. initialize принимает `logic_config` browser/formatting; общий `actions.rs` используется обеими конфигурациями, UI builders удаляются.

- [x] Написать `browser_matches_previous_buttons`, `formatting_matches_previous_buttons`, `compact_browser_reuses_identical_wasm`, `same_action_names_do_not_share_state`, `profile_switch_cancels_late_activation`. Assert current shortcut IDs и application scopes, item IDs, ширина 240 и reload asset не меняются; два одновременно открытых набора имеют разный capability/state fixture и раздельный результат.

  ```rust
  assert_eq!(browser_width, 240);
  assert_eq!(formatting_width, 240);
  assert_eq!(browser_widget_ids, vec![10, 11, 12]);
  assert_eq!(formatting_widget_ids, vec![20, 21, 22]);
  assert_eq!(wasm_digest_before_new_layout, wasm_digest_after_new_layout);
  ```
- [x] Выполнить новые headless/replay/acceptance fixtures до миграции; ожидается отсутствие declarative sets.
- [x] Перенести полный UI в два файла, действия — в новый world. Проверку keys/applications оставить в permission scope, не копировать в layout. Compact-browser — тестовый новый файл с тем же Wasm, не постоянная новая продуктовая кнопка.
- [x] Build/check/replay app-actions и host acceptance; сравнить digest Wasm при перестановке кнопок и добавлении compact set: идентичен.
- [x] Коммит: `refactor: separate app action layouts from shared logic`.

### Task 9: Controls — sliders, очереди и функции движка

**Files:** Create `plugins/controls/sets/*.toml` по текущим primary items и `src/logic.rs`, `tests/replay.json`; Modify `src/lib.rs`, `Cargo.toml`, manifest; завершить logic-sdk `src/controls.rs`.

**Interfaces:** Controls logic использует EngineApi.audio/display/power из task 2, сохраняет latest-value очередь и per-instance requests/resources. Описания panel layout живут в файле соответствующего основного набора; логика возвращает presentation commands и state patches.

- [x] Написать `drag_keeps_latest_value`, `cancel_does_not_commit_value`, `slider_release_preserves_activation`, `revocation_during_drag_blocks_write`, `hidden_controls_stop_watchers`, `brightness_device_is_bounded`, `early_events_are_instance_local`. Assert существующие command IDs, timeout/retry bounds и 32 early-events budget; pending volume у A не меняет B.
- [x] Выполнить controls/SDK tests и replay до миграции; новые layout bindings должны падать.
- [x] Перенести UI/panels в файлы; обработчики очередей остаются в логике, общие request/parse adapters — в SDK. Сохранить текущие tap/hold/anchored panel/dismissal политики и стабильные IDs. Не заменить root-owned executable проверку небезопасным fallback.
- [x] Build/check/replay controls; presentation acceptance и тест без grants проходят с предсказуемой недоступностью кнопок.
- [x] Коммит: `refactor: move controls layouts to declarative sets`.

### Task 10: Media — динамические данные и seek

**Files:** Create `plugins/media/sets/*.toml`, `src/logic.rs`, `tests/replay.json`; Modify `src/lib.rs`, `Cargo.toml`, manifest; extend renderer tests task 3 для actual media data.

**Interfaces:** Media logic использует EngineApi.media; state содержит player/track metadata, availability, playback, duration/position и seek preview. Layout задаёт transport/timeline/now-playing/list templates, не вычисляет системные команды.

- [ ] Написать `seek_keeps_original_track_id`, `track_change_cancels_seek`, `cancel_restores_preview`, `player_list_reorder_preserves_key`, `lost_player_disables_transport`, `subscription_cleanup_on_hide`. Assert SetPosition содержит правильный track ID; смена track/player отменяет устаревший seek; prev/play/next используют прежние actions.
- [ ] Выполнить media replay и renderer tests до миграции; новые bindings должны падать.
- [ ] Перенести UI в файлы, сохранить все assets/ширины/item IDs. Логика ведёт async state, не создаёт дерево. Связать MPRIS события с state patch без периодического busy polling.
- [ ] Build/check/replay media; существующие media/UI acceptance проходят. Проверить idle redraw и lifecycle подписки.
- [ ] Коммит: `refactor: separate media layouts and interaction logic`.

### Task 11: Остальные встроенные наборы

**Files:** В `plugins/{capture,hyprland,command-deck,omarchy}/` Create `sets/*.toml`, необходимые `src/logic.rs`, replay fixtures; Modify manifest/lib/Cargo.toml. Existing `plugins/omarchy/tests/replay.json` обновить, не выкидывать прежние assertions. Provider implementation не переделывать.

**Interfaces:** Тот же world; common action/command adapters выносить в библиотечные модули при фактическом повторении. Layout files соответствуют каждому существующему primary item; panel helpers остаются в том же экземпляре логики.

- [ ] Зафиксировать regression fixtures всех нынешних widget IDs/actions, widths, panels и optional grants. Написать `first_party_layouts_have_no_commands`, `omarchy_provider_is_unchanged`, `command_completion_updates_only_own_set`, `panel_close_cancels_pending_contact`. Assert каждый ID из прежнего manifest доступен, providers/scopes сохраняются.
- [ ] Выполнить новые и существующие replay tests до миграции; новые отдельные описания отсутствуют.
- [ ] Перенести layouts и action bindings, сохранив command IDs/DBus scopes и запреты запуска без разрешений. Omarchy кнопки используют новый runtime; отдельный appearance provider остаётся прежним world и entrypoint.
- [ ] Build/check/replay каждого из четырёх пакетов, выполнить применимые presentation/effect/asset tests.
- [ ] Коммит: `refactor: migrate remaining first party button sets`.

### Task 12: Документация, build tooling и полный regression

**Files:** Modify `scripts/build-first-party-packs.sh`, `scripts/test-packaging.sh`, `docs/plugin-author-context.md`, `docs/design/first-party-packs.md`, `docs/design/profile-composition.md`, `CONTRIBUTING.md`; добавить пример общего модуля/двух описаний к example task 2; update CLI `sdk_context_markdown`.

**Interfaces:** Документированное создание набора: файл → явная manifest reference → plugin check/pack/install, без пересборки неизменённого Wasm. Пользовательские композиции: root include → отдельные profile fragments. Build script включает app-actions и omarchy вместе с прежними пятью пакетами и поддерживает оба world.

- [ ] Написать acceptance `layout_only_edit_needs_no_build`, `legacy_packages_still_launch`, `denied_set_cannot_issue_shortcut`, `tampered_set_is_rejected`, `deleting_set_releases_resources`. Assert производственный host не читает изменённые исходники поверх sealed bytes; capability denial остаётся видимым и не эскалируется.
- [ ] Выполнить focused acceptance до финальных tooling changes; отсутствующие подключения кампаний должны быть видны как failure, не skip.
- [ ] Обновить docs/tooling; объяснить, какие данные относятся к каждому слою, где общий код, как отдельно хранится состояние. Не описывать runtime как полностью реализованный до завершения задач 1–11. Обновлённые first-party Wasm коммитить только как намеренные артефакты миграции, не побочные результаты сборки unrelated examples.
- [ ] Выполнить `tb_cargo fmt --all -- --check`, `tb_cargo clippy --workspace --all-targets -- -D warnings`, `tb_cargo test --workspace`, doc tests/build checks. Затем полную host packaging campaign из раздела окружения (включает применимые security/acceptance). Ignored host-only tests запустить на host согласно существующему harness. Любой failure разобраться/исправить, не ослаблять assertions.
- [ ] Провести обзор ветки с фокусом на grants/activation, sealed transport, independent Store, cancellation и обратную совместимость. Исправить найденные дефекты и повторить только затронутые проверки.
- [ ] Коммит: `docs: document button set authoring and complete migration checks`; проверить diff/status, отсутствие изменений AUR/вложенного checkout. Сообщить коммиты, проверенные сценарии и реальные ограничения; push/установка не входят в задачу.

## Самопроверка плана и передача выполнения

Все разделы spec покрыты: schema/layout — 1/3; logic и shared code — 2/4/8;
package/sealing/policy — 5/6; includes/reload — 7; текущие наборы — 8–11;
совместимость и security/regression — 12. Пять условий Review Focus имеют
именованные проверки в указанных задачах. Состояние и дерево не рекурсивны в
WIT; low-level типы не зависят от plugin-host; единицы логики и panel helpers
не создают случайного совместного Store.

Рекомендуемое выполнение: **Native**, последовательно в этом чате, с обзором
всей ветки в конце. Большинство задач зависит от общего WIT/schema/catalog,
поэтому последовательное выполнение уменьшает риск несовместимых интерфейсов
и стоимость передачи контекста. Альтернатива — Subagent-driven с отдельным
исполнителем и ревьюером на каждую задачу.

Статус: Native выполнение согласовано. Выполненные задачи отмечаются чекбоксами;
после каждой проверенной задачи создаётся коммит и push в t2-appletbdrm.
