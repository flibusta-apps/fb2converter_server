import asyncio
from concurrent.futures import ThreadPoolExecutor
import os
import time
from typing import AsyncIterator
import uuid

from fastapi import FastAPI, APIRouter, File, UploadFile, Form, HTTPException, status
from fastapi.responses import StreamingResponse

import aiofiles
import aiofiles.os
import aiofiles.ospath
from fastapi_utils.tasks import repeat_every
import sentry_sdk

from config import env_config


sentry_sdk.init(
    env_config.SENTRY_DSN,
)


router = APIRouter(tags=["converter"])


@router.post("/")
async def convert(
    file: UploadFile = File({}),
    format: str = Form({}),
):
    format_lower = format.lower()
    if format_lower not in ["epub", "mobi"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST, detail="Wrong format!"
        )

    temp_uuid = uuid.uuid1()

    temp_filename = str(temp_uuid) + ".fb2"
    converted_temp_filename = str(temp_uuid) + "." + format_lower

    try:
        async with aiofiles.open(temp_filename, "wb") as f:
            while content := await file.read(1024):
                if isinstance(content, str):
                    content = content.encode()

                await f.write(content)

        proc = await asyncio.create_subprocess_exec(
            "./bin/fb2c",
            "convert",
            "--to",
            format,
            temp_filename,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        _, stderr = await proc.communicate()
    finally:
        await aiofiles.os.remove(temp_filename)

    if proc.returncode != 0 or len(stderr) != 0:
        try:
            await aiofiles.os.remove(converted_temp_filename)
        except FileNotFoundError:
            pass

        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST, detail="Can't convert!"
        )

    async def result_iterator() -> AsyncIterator[bytes]:
        try:
            async with aiofiles.open(converted_temp_filename, "rb") as f:
                while data := await f.read(2048):
                    yield data
        finally:
            await aiofiles.os.remove(converted_temp_filename)

    return StreamingResponse(result_iterator())


@router.get("/healthcheck")
async def healthcheck():
    return "Ok!"


app = FastAPI()

app.include_router(router)


@app.on_event("startup")
@repeat_every(seconds=60 * 60)
async def remote_temp_files():
    def _foo():
        current_time = time.time()

        for f in os.listdir("/tmp/"):
            creation_time = os.path.getctime(f)
            if (current_time - creation_time) // 3600 >= 3:
                os.unlink(f)

    loop = asyncio.get_event_loop()

    with ThreadPoolExecutor(1) as executor:
        await loop.run_in_executor(executor, _foo)
