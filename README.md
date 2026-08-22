# blazy

Blender-подобный UI-слой поверх [Masonry](https://github.com/linebender/xilem):
области и сплиты окон, нод-канвас с обычными виджетами внутри нод, операторы и
keymap.

**Статус: Фаза 0.5 — проверка гипотезы.** Ничего готового к использованию пока нет.
Фаза 0 (нод-канвас) и Фаза 0.5 (области) измерены, критерии обеих держатся в CI.

Архитектура и результаты замеров — [`rnd/architecture.md`](rnd/architecture.md).

## Структура

| Крейт | Назначение |
|---|---|
| `crates/blazy` | Фасад (пока пустой) |
| `crates/blazy-canvas` | Канвас с паном, зумом, culling и LOD |
| `crates/blazy-areas` | Области экрана: дерево сплитов над одним деревом виджетов |
| `crates/bench-utils` | Критерии как проверяемые утверждения, вердикт, JSON-отчёт |
| `examples/node-canvas` | Эксперимент Фазы 0: нод-канвас, замеры и критерии |
| `examples/area-screen` | Эксперимент Фазы 0.5: экран из областей, замеры и критерии |

## Быстрый старт

```bash
cargo make run-node-canvas   # окно с 5000 нод
cargo make run-area-screen   # окно, разбитое на области
cargo make bench             # замеры и критерии Фазы 0
cargo make bench-areas       # замеры и критерии Фазы 0.5
cargo make ci                # то же, что гоняет CI: lint + тесты + оба гейта
```

Критерии Фазы 0 — гейт, а не абзац в документе: `cargo make bench` завершается
ненулевым кодом, если хоть один перестал выполняться. Гейтятся детерминированные
счётчики, а не миллисекунды; почему именно так — `crates/bench-utils/src/criteria.rs`,
почему не `criterion` — `examples/node-canvas/benches/phase0/main.rs`.

## Зависимости

`masonry` подключён как **git-зависимость с пиннингом по коммиту**. Всё, на чём
строится blazy — рендерный IR `imaging`, `Widget::paint(&mut Painter)`,
`VisualLayerPlan` — существует только в git-main: опубликованный `masonry` 0.4.0
(2025-10-29) старше миграции на `imaging`
([xilem#1696](https://github.com/linebender/xilem/pull/1696), мерж 2026-03-24).
Подробности и способы смягчения — `rnd/architecture.md` §15.1.

## License

Licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE)
  or [apache.org/licenses/LICENSE-2.0](https://www.apache.org/licenses/LICENSE-2.0))
- MIT license ([LICENSE-MIT](LICENSE-MIT) or [opensource.org/licenses/MIT](https://opensource.org/licenses/MIT))

at your option.
