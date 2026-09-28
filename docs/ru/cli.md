[Обзор](../../README.ru.md) · [Веб](web.md) · **Приложение и CLI** · [Python](python.md) · [Docker](docker.md) · [HTTP API](http-api.md) · [Приватность](privacy.md) · [Скорость](performance.md) · [Формат выгрузки](format.md)

[English](../cli.md) · Русский

# Приложение и командная строка

Это одна программа. Скачайте её для
[Windows](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-windows-x64.zip),
[macOS (Apple silicon)](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-macos-arm64.tar.gz),
[macOS (Intel)](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-macos-x64.tar.gz) или
[Linux](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-linux-x64.tar.gz).

## Приложение

Распакуйте архив и дважды щёлкните `sap-bin`. В браузере откроется [веб-страница](web.md),
которую отдаёт ваш собственный компьютер: ничего не покидает его, ограничения на размер нет,
и программа использует все ядра процессора. Чтобы остановить её, закройте окно или нажмите
Ctrl+C.

В первый раз macOS может отказаться запускать приложение из интернета: щёлкните `sap-bin`
правой кнопкой и выберите *Открыть*. Если Windows SmartScreen показывает предупреждение,
нажмите *Подробнее*, затем *Выполнить в любом случае*.

## Команды

```bash
sap-bin info BSIS.QUERY.zip --fields          # что внутри этой выгрузки?
sap-bin head BSIS.QUERY.zip -n 3              # первые три записи в расшифрованном виде
sap-bin convert BSIS.QUERY.zip -o bsis.parquet
sap-bin convert BSIS.QUERY/ -o bsis.csv       # распакованная папка выгрузки
sap-bin convert DATA.1.BIN DATA.2.BIN --schema DATA.0.TXT -o bsis.csv
sap-bin convert BSIS.QUERY.zip -o shards/ --split -f parquet
cat BSIS.QUERY.zip | sap-bin convert - -o - > bsis.csv
sap-bin probe DATA.1.BIN --schema DATA.0.TXT  # если записи не совпадают со схемой
sap-bin bench                                 # замерить скорость на этом компьютере
```

| Команда | |
|---|---|
| `sap-bin` или `sap-bin app` | открыть приложение в браузере |
| `sap-bin serve` | запустить веб-сервис (настройки описаны в разделе [Docker](docker.md)) |
| `sap-bin info PATH` | схема, геометрия записи, число шардов; `--fields` перечисляет все поля со смещениями |
| `sap-bin head PATH` | расшифровать и вывести первые записи (`-n`) |
| `sap-bin probe PATH` | оценить варианты размера записи, если данные не совпадают со схемой |
| `sap-bin convert PATH… -o OUT` | конвертировать; `PATH` — это `.zip`, папка, один или несколько файлов `.BIN`/`.TXT` или `-` для stdin |
| `sap-bin bench` | скорость на синтетических данных |

## Параметры convert

| Флаг | |
|---|---|
| `-f`, `--format` | `csv` (по умолчанию), `tsv`, `jsonl`, `parquet` или `arrow` (поток Arrow IPC для pandas, Polars или DuckDB) |
| `-o`, `--output` | файл, `-` для stdout или папка при `--split` |
| `--schema DATA.0.TXT` | использовать эту схему вместо схемы выгрузки (подходит и `DATA.0.zip`) |
| `--split` | отдельный выходной файл на каждый шард |
| `--limit N` | остановиться после N записей (для каждого шарда при `--split`) |
| `--record-size N` | заменить размер записи, заданный схемой |
| `--float-decimals` | суммы как float64 вместо точных десятичных |
| `--on-error skip` | оставлять нерасшифровываемые значения пустыми, подсчитывать их и продолжать |
| `--delimiter`, `--encoding` | разделитель CSV; `--encoding utf-8-sig` добавляет BOM, без которого Excel неверно показывает кириллицу |
| `--compression` | сжатие Parquet: `zstd` (по умолчанию), `snappy`, `gzip`, `none` |
| `--text-encoding` | кодировка текстовых шардов `.TXT` (по умолчанию windows-1251) |
| `--threads N` | число рабочих потоков (по умолчанию по одному на ядро) |

`sap-bin <команда> --help` перечисляет всё. Коды выхода: 0 при успехе, 1 — если запись не
удаётся расшифровать, 2 — при ошибке в схеме, архиве или аргументах.
