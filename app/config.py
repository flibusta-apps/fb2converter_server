from typing import Optional

from pydantic import BaseSettings


class EnvConfig(BaseSettings):
    SENTRY_DSN: Optional[str]


env_config = EnvConfig()
