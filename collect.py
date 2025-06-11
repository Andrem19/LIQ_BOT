import os
import re
from datetime import datetime

# Название директории
TARGET_DIR = "./"

# Директории, которые полностью исключаются из обхода (и структуры, и кода)
EXCLUDED_DIRS = {".git", "target", "venv", "vendor", "__pycache__"}

# Директории/файлы, из которых только КОД исключается (но структура остаётся)
EXCLUDED_CODE_DIRS = {
    os.path.normpath("src/decoders/phoenix.rs"),
    os.path.normpath(".gitignore"),
    os.path.normpath("./collect.py"),
    os.path.normpath("code_map"),
    os.path.normpath(".env"),
    os.path.normpath("graph.dot"),
    os.path.normpath("solana_pools.json"),
    os.path.normpath("src/visualizer"),
    os.path.normpath("src/exchange"),
}

# Пути (файлы или директории), где нужно выводить только сигнатуры (Rust)
SIGNATURE_ONLY_FILES_OR_DIRS = {
    # os.path.normpath("src/notify.rs"),
    # os.path.normpath("src/config.rs"),
}

# Файлы, которые полностью исключаются из карты (структуры и кода)
EXCLUDED_FILES = {
    os.path.normpath("collect.py"),
    os.path.normpath("Cargo.lock"),
    os.path.normpath("test1.py"),
    os.path.normpath("test2.py"),
    os.path.normpath("test3.py"),
    os.path.normpath("test4.py"),
    os.path.normpath("graph.png"),
}

# Префиксы имён файлов, которые нужно полностью исключить
EXCLUDED_FILE_PREFIXES = (
    "code_map",
    "vendor",
    "test",
)

# === Новый список: если он НЕ пуст, обрабатываются только эти файлы/директории ===
INCLUDE_ONLY_FILES_OR_DIRS = [
    # Пример:
    # os.path.normpath("src/main.rs"),
    # os.path.normpath("src/arb"),
    # os.path.normpath("src/rpc"),
]


def is_included(rel_path: str) -> bool:
    """
    Проверяет, нужно ли обрабатывать данный путь (относительно TARGET_DIR) с учётом INCLUDE_ONLY_FILES_OR_DIRS.
    - Если INCLUDE_ONLY_FILES_OR_DIRS пуст, возвращает True для любых rel_path.
    - Если rel_path == '' (корень), возвращает True, чтобы искать внутри.
    - Иначе возвращает True, если:
        * rel_path точно равно одному из указанных в INCLUDE_ONLY_FILES_OR_DIRS, или
        * rel_path — предок какого-либо указанного пути (из INCLUDE_ONLY_FILES_OR_DIRS), или
        * rel_path — потомок (внутри) какого-либо указанного пути.
    """
    if not INCLUDE_ONLY_FILES_OR_DIRS:
        return True

    # Нормализуем входной путь (чтобы не было './' и т.п.)
    rel_path_norm = os.path.normpath(rel_path)
    # Считаем, что пустая строка '' (корень) — нужен, чтобы дальше искать указанные пути
    if rel_path_norm == "" or rel_path_norm == ".":
        return True

    for incl in INCLUDE_ONLY_FILES_OR_DIRS:
        # если rel_path точно совпадает с указанным
        if rel_path_norm == incl:
            return True
        # если rel_path — предок указанного (например, rel_path='src', incl='src/decoders')
        if incl.startswith(rel_path_norm + os.sep):
            return True
        # если rel_path — потомок указанного (например, rel_path='src/decoders/parser.rs', incl='src/decoders')
        if rel_path_norm.startswith(incl + os.sep):
            return True

    return False


def build_project_map(directory: str):
    """
    Строит карту проекта: возвращает список (относительный путь, абсолютный путь),
    пропуская файлы из EXCLUDED_FILES и файлы, имена которых начинаются с префиксов из EXCLUDED_FILE_PREFIXES.
    Если INCLUDE_ONLY_FILES_OR_DIRS не пуст, включаются только пути, соответствующие правилам is_included().
    """
    file_list = []
    for root, dirs, files in os.walk(directory):
        # Убираем каталоги, которые полностью исключаем из начала
        dirs[:] = [d for d in dirs if d not in EXCLUDED_DIRS]

        # Получаем относительный путь к текущей папке
        rel_root = os.path.relpath(root, start=directory)
        if rel_root == ".":
            rel_root = ""  # корень обозначаем как пустую строку

        # Если текущая папка НЕ входит в INCLUDE_ONLY_FILES_OR_DIRS (и не является её предком/потомком), пропускаем
        if not is_included(rel_root):
            # чтобы не заходить глубже по этой ветке
            dirs[:] = []
            continue

        # Фильтруем файлы в этой папке
        for f in files:
            norm_f = os.path.normpath(f)
            # Относительный путь к файлу (например, "src/main.rs")
            rel_file = os.path.normpath(os.path.join(rel_root, f)) if rel_root else f

            # Полные исключения по имени и по префиксу
            if norm_f in EXCLUDED_FILES:
                continue
            if any(f.startswith(prefix) for prefix in EXCLUDED_FILE_PREFIXES):
                continue

            # Если включающий список НЕ пуст и данный файл НЕ попадает под правила is_included, пропускаем
            if not is_included(rel_file):
                continue

            # Всё ок: добавляем в результат
            full_path = os.path.join(root, f)
            file_list.append((rel_file, full_path))

    return file_list


def read_file_content(filepath: str) -> str:
    """
    Читает содержимое файла, убирая полностью закомментированные строки ('//') и пустые строки,
    оставляя только содержательный код.
    """
    lines = []
    try:
        with open(filepath, "r", encoding="utf-8") as f:
            for line in f:
                stripped = line.lstrip()
                if stripped.startswith("//") or stripped.strip() == "":
                    continue
                lines.append(line.rstrip())
    except Exception as e:
        return f"[Ошибка чтения файла: {e}]"
    return "\n".join(lines)


def extract_rust_signatures(filepath: str) -> str:
    """
    Извлекает только сигнатуры функций, async-функций, struct, enum, trait, impl для Rust-файла.
    """
    try:
        with open(filepath, "r", encoding="utf-8") as f:
            code = f.read()
    except Exception as e:
        return f"[Ошибка чтения файла: {e}]"

    signatures = []
    struct_enum_trait_re = re.compile(r"^\s*(pub\s+)?(struct|enum|trait|impl)[^;{]*[{]", re.MULTILINE)
    fn_re = re.compile(r"^\s*(pub\s+)?(async\s+)?(unsafe\s+)?fn\s+[^\(]+\([^\)]*\)[^\{;]*[{]", re.MULTILINE)

    for match in struct_enum_trait_re.finditer(code):
        header = match.group().split("{")[0].strip()
        signatures.append(header + " { ... }")

    for match in fn_re.finditer(code):
        header = match.group().split("{")[0].strip()
        # убираем атрибуты #[…]
        header = re.sub(r'#\[[^\]]*\]\s*', '', header)
        header = header.rstrip(";")
        signatures.append(header + ";")

    # Убираем дубли
    signatures = list(dict.fromkeys(signatures))

    return "\n".join(signatures) if signatures else "[Сигнатуры не найдены]"


def generate_folder_tree(directory: str) -> str:
    """
    Генерирует текстовую карту структуры папок и файлов,
    учитывая INCLUDE_ONLY_FILES_OR_DIRS и EXCLUDED_DIRS/EXCLUDED_FILES/EXCLUDED_FILE_PREFIXES.
    """
    lines = []
    for root, dirs, files in os.walk(directory):
        # Сначала отсеиваем папки, которые полностью исключены
        dirs[:] = [d for d in dirs if d not in EXCLUDED_DIRS]

        rel_root = os.path.relpath(root, start=directory)
        if rel_root == ".":
            rel_root = ""

        # Если данная папка НЕ входит в INCLUDE_ONLY_FILES_OR_DIRS (и не её предок/потомок), пропускаем
        if not is_included(rel_root):
            dirs[:] = []
            continue

        level = rel_root.count(os.sep) if rel_root else 0
        indent = "    " * level
        folder_name = os.path.basename(root) if rel_root else os.path.basename(os.path.abspath(directory))
        lines.append(f"{indent}{folder_name}/")

        # Теперь проходим по файлам в этой папке
        for f in files:
            norm_f = os.path.normpath(f)
            rel_file = os.path.normpath(os.path.join(rel_root, f)) if rel_root else f

            # Полное исключение по имени или префиксу
            if norm_f in EXCLUDED_FILES:
                continue
            if any(f.startswith(prefix) for prefix in EXCLUDED_FILE_PREFIXES):
                continue

            # Если включающий список НЕ пуст и файл НЕ попадает под правила is_included, пропускаем
            if not is_included(rel_file):
                continue

            lines.append(f"{indent}    {f}")

    return "\n".join(lines)


def is_in_excluded_code_dir(rel_path: str) -> bool:
    """
    Проверяет, входит ли файл в папку, из которой надо исключить код.
    """
    rel_path_norm = os.path.normpath(rel_path)
    for excluded in EXCLUDED_CODE_DIRS:
        if rel_path_norm.startswith(excluded):
            return True
    return False


def is_in_signature_only_dir(rel_path: str) -> bool:
    """
    Проверяет, входит ли файл в папку/файл, для которого выводим только сигнатуры.
    """
    rel_path_norm = os.path.normpath(rel_path)
    for sig_path in SIGNATURE_ONLY_FILES_OR_DIRS:
        if rel_path_norm == sig_path or rel_path_norm.startswith(sig_path + os.sep):
            return True
    return False


def estimate_gpt_tokens(text: str) -> int:
    """
    Приближённая оценка числа токенов GPT на входе (1 токен ≈ 4 символа).
    """
    return int(len(text) / 4)


def main():
    now = datetime.now().strftime("%Y-%m-%d_%H-%M-%S")
    output_filename = f"code_map_{now}.txt"

    if not os.path.isdir(TARGET_DIR):
        print(f"Ошибка: директория '{TARGET_DIR}' не найдена.")
        return

    files = build_project_map(TARGET_DIR)
    folder_tree = generate_folder_tree(TARGET_DIR)

    with open(output_filename, "w", encoding="utf-8") as out:
        out.write("===== КАРТА СТРУКТУРЫ ПРОЕКТА =====\n\n")
        out.write(folder_tree)
        out.write("\n\n===== КОД ФАЙЛОВ =====\n\n")

        for rel_path, full_path in files:
            out.write(f"{TARGET_DIR}/{rel_path}:\n")
            if is_in_excluded_code_dir(rel_path):
                out.write("[КОД ИСКЛЮЧЕН]\n")
            elif is_in_signature_only_dir(rel_path):
                out.write(extract_rust_signatures(full_path))
            else:
                out.write(read_file_content(full_path))
            out.write("\n" + "-" * 80 + "\n")

    print(f"✅ Карта проекта с кодом успешно сохранена в файл '{output_filename}'.")

    try:
        with open(output_filename, "r", encoding="utf-8") as f:
            contents = f.read()
            token_estimate = estimate_gpt_tokens(contents)
            print(f"Оценочное количество токенов для GPT: {token_estimate}")
    except Exception as e:
        print(f"Ошибка при оценке токенов: {e}")


if __name__ == "__main__":
    main()
