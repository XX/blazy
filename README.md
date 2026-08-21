# blazy

Blender-подобный UI-слой поверх [Masonry](https://github.com/linebender/xilem):
области и сплиты окон, нод-канвас с обычными виджетами внутри нод, операторы и
keymap.

**Статус: Фаза 0 — проверка гипотезы.** Ничего готового к использованию пока нет.

Архитектура и результаты замеров — [`rnd/architecture.md`](rnd/architecture.md).

## Структура

| Крейт | Назначение |
|---|---|
| `crates/blazy` | Фасад (пока пустой) |
| `crates/blazy-canvas` | Канвас с паном, зумом, culling и LOD |
| `examples/node-canvas` | Эксперимент Фазы 0 с замерами и критериями |

## Быстрый старт

```bash
cargo make run-node-canvas   # окно с 5000 нод
cargo make bench             # замеры и критерии Фазы 0
cargo make ci                # то же, что гоняет CI: lint + тесты + критерии
```

Критерии Фазы 0 — гейт, а не абзац в документе: `cargo make bench` завершается
ненулевым кодом, если хоть один перестал выполняться. Гейтятся детерминированные
счётчики, а не миллисекунды; почему именно так — `examples/node-canvas/src/criteria.rs`.

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
