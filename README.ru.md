# sap-bin

[English](README.md) · Русский

**Конвертирует двоичные выгрузки таблиц SAP (`.BIN`) в CSV, Parquet или JSON Lines.**
Достаточно быстро для сотни миллионов записей, достаточно просто, чтобы перетащить файл на
веб-страницу, и устроено так, что ничего из конвертируемого не сохраняется.

[![CI](https://github.com/aerofeev/sap-bin-parser/actions/workflows/ci.yml/badge.svg)](https://github.com/aerofeev/sap-bin-parser/actions/workflows/ci.yml)
[![MIT licence](https://img.shields.io/badge/licence-MIT-blue.svg)](LICENSE)

Когда вы выгружаете из SAP таблицу или результат запроса в двоичном виде, вы получаете
zip-архив из zip-архивов по шардам с файлами `DATA.N.BIN`. В них нет ни разделителей, ни
строки заголовка, ни явной границы между записями: суммы хранятся упакованными десятичными
числами (COMP-3), а текст — в UTF-16 big-endian. В текстовом редакторе такой файл выглядит
как вперемешку нулевые байты и ничего полезного.

Эта программа их читает.

![Просмотр выгрузки: таблица, геометрия записи и первые записи](docs/images/inspect.png)

## Три способа пользоваться

**В браузере.** Откройте **[tools.eidox.io/sap-bin-parser](https://tools.eidox.io/sap-bin-parser/)**,
перетащите выгрузку, посмотрите на первые записи, конвертируйте. В современном браузере
конвертация идёт прямо на странице, так что файл никуда не отправляется, а *Explore
records* открывает его в просмотрщике таблиц [Perspective](https://perspective-dev.github.io),
где записи можно сортировать, фильтровать, сводить и строить по ним графики. Страница
принимает исходный `.zip`, распакованную папку или отдельные файлы `.BIN`, а для файлов без
схемы есть редактор схемы. [Подробнее о странице](docs/ru/web.md)

**Скачайте приложение** для [Windows](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-windows-x64.zip),
[macOS (Apple silicon)](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-macos-arm64.tar.gz),
[macOS (Intel)](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-macos-x64.tar.gz) или [Linux](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin-linux-x64.tar.gz) и
запустите двойным щелчком: та же страница, но на вашем компьютере и без ограничений на
размер. Это ещё и инструмент командной строки. [Приложение и CLI](docs/ru/cli.md)

```bash
sap-bin convert BSIS.QUERY.zip -o bsis.parquet
```

Удобнее Python? [`sap-bin.pyz`](https://github.com/aerofeev/sap-bin-parser/releases/latest/download/sap-bin.pyz) — один файл, которому нужен только
Python 3.10, а пакет можно использовать как библиотеку в своих конвейерах.
[Python](docs/ru/python.md)

**Запустите контейнер** на своём сервере:

```bash
docker run --rm --read-only -p 127.0.0.1:8080:8080 ghcr.io/aerofeev/sap-bin-parser
```

[Docker и собственный сервер](docs/ru/docker.md)

## Документация

| | |
|---|---|
| [Веб-страница](docs/ru/web.md) | что принимает, редактор схемы, параметры |
| [Приложение и CLI](docs/ru/cli.md) | загрузки, команды и флаги |
| [Python](docs/ru/python.md) | скрипт в одном файле и библиотека |
| [Docker и собственный сервер](docs/ru/docker.md) | образ, настройки, обратные прокси |
| [HTTP API](docs/ru/http-api.md) | конвертация из скриптов, одним запросом или по частям |
| [Ничего не сохраняется](docs/ru/privacy.md) | что хранит сервис и как это проверяется |
| [Скорость](docs/ru/performance.md) | измеренная скорость и память |
| [Формат выгрузки](docs/ru/format.md) | упакованные десятичные, UTF-16 и ловушка с выравниванием в один байт |

Для разработчиков, на английском: [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) ·
[Changelog](CHANGELOG.md) · [Почему так устроено](docs/PRODUCT.md)

## Лицензия

MIT. Сделано в eidox ai.
