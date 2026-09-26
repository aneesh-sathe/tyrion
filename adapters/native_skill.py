"""Shared exact package identity for Tyrion's native Skill adapters."""

import hashlib
import os


# Byproducts of running code, never Worker output: the caches a language writes
# when tests run. A Worker that runs its tests must not fail its Assignment
# because the interpreter left files behind in a repository with no
# .gitignore yet. Excluding them in the clone covers the adapter's staging and
# the model's own git commands, and never hides a tracked file or a real
# out-of-scope change. Tyrion's built-in Codex path repeats this list.
RUNTIME_BYPRODUCTS = (
    "__pycache__/",
    "*.py[cod]",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    "node_modules/",
    ".DS_Store",
)


def exclude_runtime_byproducts(repository):
    path = os.path.join(repository, ".git", "info", "exclude")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a", encoding="utf-8") as exclude:
        exclude.write("\n".join(RUNTIME_BYPRODUCTS) + "\n")


class RequiredSkillFailure(RuntimeError):
    def __init__(self, skill, message):
        super().__init__(message)
        self.skill = skill

    def event(self):
        return {
            "type": "tyrion.adapter.unavailable",
            "code": "required_skill_failure",
            "skill": {
                "name": self.skill["name"],
                "content_digest": self.skill["content_digest"],
            },
            "message": str(self),
        }


def skill_content_digest(skill_path):
    root = os.path.dirname(skill_path)
    files = []
    for directory, directories, names in os.walk(root, followlinks=False):
        for name in directories:
            path = os.path.join(directory, name)
            if os.path.islink(path):
                raise RuntimeError(f"Skill package contains unsupported entry: {path}")
        directories.sort()
        names.sort()
        for name in names:
            path = os.path.join(directory, name)
            if os.path.islink(path) or not os.path.isfile(path):
                raise RuntimeError(f"Skill package contains unsupported entry: {path}")
            files.append(path)
    digest = hashlib.sha256()
    for path in sorted(files, key=lambda item: os.path.relpath(item, root)):
        relative = os.path.relpath(path, root)
        with open(path, "rb") as source:
            content = source.read()
        digest.update(relative.encode())
        digest.update(b"\0")
        digest.update(b"1" if os.stat(path).st_mode & 0o111 else b"0")
        digest.update(b"\0")
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return "sha256:" + digest.hexdigest()
