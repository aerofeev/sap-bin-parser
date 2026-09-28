[Обзор](../../README.ru.md) · [Веб](web.md) · [Приложение и CLI](cli.md) · **Python** · [Docker](docker.md) · [HTTP API](http-api.md) · [Приватность](privacy.md) · [Скорость](performance.md) · [Формат выгрузки](format.md)

[English](../python.md) · Русский

# Python

Пакет на Python — эталонная реализация: [приложение](cli.md) работает в 15–25 раз быстрее,
и тесты проверяют, что результат у них совпадает.

## Скрипт в одном файле

[`sap-bin.pyz`](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin.pyz) — вся реализация на Python в одном файле размером 60 КБ.
Нужен только Python 3.10 или новее:

```bash
python sap-bin.pyz convert BSIS.QUERY.zip -o bsis.csv
python sap-bin.pyz info BSIS.QUERY.zip --fields
python sap-bin.pyz head BSIS.QUERY.zip -n 3
```

В нём есть команды `info`, `head`, `probe` и `convert`, как в [приложении](cli.md), для
`.zip` или одного `.BIN` с `--schema`. Для вывода в Parquet сначала выполните
`pip install pyarrow`.

## Библиотека

```bash
pip install 'sap-bin-parser[parquet] @ git+https://github.com/aerofeev/sap-bin-parser'
```

Так же устанавливается и команда `sap-bin-py`.

```python
from sap_bin_parser import SapArchive, BinReader, write_parquet

with SapArchive("BSIS.QUERY.zip") as archive:
    schema = archive.schema()
    print(f"{len(schema)} полей, {schema.record_size} байт на запись")

    for shard, stream in archive.iter_shard_streams():
        write_parquet(BinReader(stream, schema), f"{shard.name}.parquet", schema)
```

Отдельный `.BIN` вместе с его файлом схемы:

```python
from sap_bin_parser import BinReader, load_schema

schema = load_schema("DATA.0.TXT")
for row in BinReader("DATA.1.BIN", schema):
    print(row["BELNR"], row["DMBTR"])
```

Схема, заданная в коде, если файла схемы нет:

```python
from sap_bin_parser import schema_from_tuples

schema = schema_from_tuples([
    ("BUKRS", "C", 4, 0, 8),
    ("DMBTR", "P", 7, 2, 7),
])
```

Записи возвращаются как словари. Суммы превращаются в `Decimal` и попадают в Parquet как
`decimal128`, поэтому значение, которое SAP записал как `0.07`, так и остаётся `0.07`;
передайте `decimal_as_float=True`, чтобы получить float64. Даты и время возвращаются как
текст в формате ISO (`2025-06-16`, `14:30:05`), а пустые даты — как `None`. Для вывода в CSV
никакие зависимости не нужны.
