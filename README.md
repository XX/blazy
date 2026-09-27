# blazy

Blender-подобный UI-слой поверх [Masonry](https://github.com/linebender/xilem):
области и сплиты окон, нод-канвас с обычными виджетами внутри нод, операторы и
keymap.

**Статус: проверка гипотезы.** Ничего готового к использованию пока нет. Нод-канвас
(Фаза 0), области (0.5), регионы и `ui_scale` (0.6), рендер и хост (Фаза 1), операторы
и keymap, операции над областями и воркспейс — измерены, и критерии держатся в CI.

Архитектура и результаты замеров — [`rnd/architecture.md`](rnd/architecture.md).

## Структура

| Крейт | Назначение |
|---|---|
| `crates/blazy` | Фасад — то, на что садится приложение; реэкспортирует остальное |
| `crates/blazy-canvas` | Канвас: пан, зум, виртуализация, LOD, связи, пространственный индекс |
| `crates/blazy-areas` | Области экрана: дерево сплитов, регионы, `ui_scale`, join/maximize/swap, воркспейс |
| `crates/blazy-ops` | Операторы, keymap как данные, модальный стек, журнал undo |
| `crates/blazy-node-editor` | Редактор нод поверх канваса: операторы выделения и правки, undo, сессия области и виджет-драйвер |
| `crates/blazy-shape` | Точный хит-тест: по фигуре, а не по прямоугольнику |
| `crates/blazy-shell` | Хост: окна, цикл событий, композиция, выбор растеризатора |
| `crates/bench-utils` | Критерии, вердикт, JSON-отчёт и метрики рендера |
| `examples/node-canvas` | Эксперимент Фазы 0: граф, виджеты нод, замеры и критерии |
| `examples/area-screen` | Эксперименты Фаз 0.5 и 0.6: области, регионы, замеры и критерии |
| `examples/hello` | Самое маленькое приложение: две области, два канваса, одна зависимость — `blazy` |

## Быстрый старт

```bash
cargo make run-node-canvas   # окно с 5000 нод
cargo make run-area-screen   # окно, разбитое на области
cargo make run-hello         # минимальное приложение только через фасад
cargo make bench-canvas      # замеры и критерии Фазы 0
cargo make bench-areas       # замеры и критерии Фаз 0.5 и 0.6
cargo make bench-shell       # замеры и критерии хоста
cargo make bench             # все три набора замеров
cargo make ci                # то же, что гоняет CI: lint + тесты + все гейты
```

Критерии — гейт, а не абзац в документе: `cargo make bench` завершается
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

Сейчас пин указывает на форк `XX/xilem`: это апстрим плюс один коммит — флаг возврата у
`Layer::capture_pointer_event`, предложенный наверх (`rnd/architecture.md` §17, п. 4).
Пин вернётся на апстрим, когда просьбу примут или отклонят.

## License

Licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE)
  or [apache.org/licenses/LICENSE-2.0](https://www.apache.org/licenses/LICENSE-2.0))
- MIT license ([LICENSE-MIT](LICENSE-MIT) or [opensource.org/licenses/MIT](https://opensource.org/licenses/MIT))

at your option.
