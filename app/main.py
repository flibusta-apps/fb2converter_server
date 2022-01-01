import asyncio
import os
import uuid

from fastapi import (
    FastAPI,
    APIRouter,
    File,
    UploadFile,
    Form,
    HTTPException,
    BackgroundTasks,
    status,
)
from fastapi.responses import FileResponse

from starlette.background import BackgroundTask

import aiofiles
import aiofiles.ospath


router = APIRouter(tags=["converter"])


@router.post("/")
async def convert(
    background_tasks: BackgroundTasks,
    file: UploadFile = File({}),
    format: str = Form({}),
):
    temp_uuid = uuid.uuid1()

    temp_filename = str(temp_uuid) + ".fb2"
    converted_temp_filename = str(temp_uuid) + "." + format

    async with aiofiles.open(temp_filename, "wb") as f:
        content = await file.read()

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

    background_tasks.add_task(os.remove, temp_filename)

    if proc.returncode != 0 or len(stderr) != 0:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST, detail="Can't convert!"
        )

    return FileResponse(
        converted_temp_filename,
        background=BackgroundTask(lambda: os.remove(converted_temp_filename)),
    )


app = FastAPI()

app.include_router(router)
