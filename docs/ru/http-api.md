[Обзор](../../README.ru.md) · [Веб](web.md) · [Приложение и CLI](cli.md) · [Python](python.md) · [Docker](docker.md) · **HTTP API** · [Приватность](privacy.md) · [Скорость](performance.md) · [Формат выгрузки](format.md)

[English](../http-api.md) · Русский

# HTTP API

Страница сервиса пользуется этими адресами, и скрипты тоже могут. Пути указаны
относительно корня сервиса, например `https://tools.eidox.io/sap-bin-parser/`.

## Конвертация одним запросом

```bash
curl -fsS --data-binary @BSIS.QUERY.zip 'http://localhost:8080/api/convert?format=parquet' -o bsis.parquet

curl -fsS -F schema=@DATA.0.TXT -F file=@DATA.1.BIN -F file=@DATA.2.BIN \
  'http://localhost:8080/api/convert?multi=true&format=csv' -o bsis.csv
```

Тело запроса — сама выгрузка или multipart-форма с необязательной частью `schema`, за
которой идёт одна часть `file` (несколько при `multi=true`). В ответ приходит
сконвертированный файл. Ошибки, найденные до начала вывода (неверная схема, неправильный
размер записи), возвращаются в виде JSON с кодом 400, 413 или 422. Если сбой происходит
после начала вывода, ответ обрывается, чтобы неполный файл никогда не выглядел законченным.

Один запрос одновременно отправляет и получает данные. curl это умеет, а браузеры и многие
прокси — нет, и через них большая конвертация зависает. Через прокси используйте задания с
загрузкой по частям, описанные ниже.

## Конвертация заданием, по частям

Так работает сама страница. Ни одной стороне не нужно отправлять и получать данные в одном
запросе, и ни один запрос не больше 8 МБ, поэтому ограничения прокси на размер загрузки не
мешают.

```bash
base=http://localhost:8080 id=my-job-0001
curl -fsS -X POST --data-binary @DATA.0.TXT "$base/api/jobs?job=$id&format=csv"  # тело: схема или пусто
curl -fsS "$base/api/jobs/$id/download" -o bsis.csv &                             # результат
split -b 8m BSIS.QUERY.zip part.                                                  # входные данные по порядку
for p in part.*; do curl -fsS -X POST --data-binary @"$p" "$base/api/jobs/$id/input"; done
curl -fsS -X POST "$base/api/jobs/$id/input?end=true"
wait
```

Каждый запрос `input` получает ответ, только когда конвертер принял эту часть, поэтому
загрузка идёт не быстрее конвертации, а память остаётся ограниченной. Для задания из
нескольких файлов (`multi=true`) первая часть каждого файла передаётся с
`?start=true&name=DATA.1.BIN`. Если загрузка молчит десять минут, задание отменяется.

## Адреса

| | |
|---|---|
| `POST api/convert` | конвертация одним запросом |
| `POST api/jobs?job=ID` | создать задание с загрузкой по частям (ID выбираете вы: от 8 до 64 латинских букв, цифр или `-`) |
| `POST api/jobs/{id}/input` | следующая часть; `start`, `name`, `end` — как описано выше |
| `GET api/jobs/{id}/download` | результат, потоком |
| `GET api/jobs/{id}` | ход работы: состояние, прочитано байт, записано записей, готово шардов |
| `POST api/jobs/{id}/cancel` | остановить |
| `POST api/inspect` | описать выгрузку по multipart-части `head` (первые байты файла) и, по желанию, `tail`, `size`, `schema`, `record_size` |
| `GET api/sample` | синтетическая выгрузка (`records`, `shards`) |
| `GET api/config`, `GET healthz` | версия и ограничения |
| `POST api/usage` | итоги конвертации, которую страница выполнила в браузере, в JSON (`event`, `format`, `input`, `table`, `records`, `shards`, `bytes_in`, `bytes_out`, `seconds`), для статистики использования |
| `GET api/stats`, `GET metrics` | статистика использования в JSON или для Prometheus; с токеном оператора (`Authorization: Bearer ...`) и только если он задан |

CSV, TSV и JSON Lines передаются сжатыми, если запрос это допускает (`Accept-Encoding`:
zstd, иначе gzip); браузеры распаковывают их при сохранении. `curl` получает обычный файл,
если не указать `--compressed`, а на медленном канале это стоит делать. Parquet и zip с
шардами уже сжаты и передаются как есть. Для задания сжатие определяется заголовком
`Accept-Encoding` запроса, который его создал.

## Параметры

`api/convert` и `api/jobs` принимают одинаковые параметры запроса, повторяющие
[флаги командной строки](cli.md#параметры-convert):

| | |
|---|---|
| `format` | `csv`, `tsv`, `jsonl`, `parquet`, `arrow` (поток Arrow IPC) |
| `split` | `true`: zip-архив с отдельным файлом на каждый шард |
| `limit` | остановиться после стольких записей |
| `record_size` | заменить размер записи из схемы |
| `decimals` | `float` — суммы как float64 |
| `on_error` | `skip` — оставлять нерасшифровываемые значения пустыми |
| `delimiter`, `bom` | разделитель CSV; `bom=true` для Excel |
| `compression` | сжатие Parquet |
| `text_encoding` | кодировка шардов `.TXT` |
| `multi` | `true`: каждый загруженный файл — отдельная часть выгрузки |
| `name` | название выгрузки, для имени выходного файла |
| `job` | идентификатор для отслеживания хода; обязателен для `api/jobs` |
