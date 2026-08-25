"""Shared exact package identity for Tyrion's native Skill adapters."""

import hashlib
import os


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
