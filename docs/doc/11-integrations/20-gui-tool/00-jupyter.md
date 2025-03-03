---
title: Jupyter Notebook
sidebar_label: Jupyter Notebook
description:
  Integrating Databend with Jupyter Notebook.
---

[Jupyter Notebook](https://jupyter.org) is a web-based interactive application that enables you to create notebook documents that feature live code, interactive plots, widgets, equations, images, etc., and share these documents easily. It is also quite versatile as it can support many programming languages via kernels such as Julia, Python, Ruby, Scala, Haskell, and R.

With the SQLAlchemy library in Python, you can establish a connection to Databend within a Jupyter Notebook, allowing you to execute queries and visualize your data from Databend directly in the Notebook.

## Tutorial: Integrate with Jupyter Notebook

In this tutorial, you will first deploy a local Databend instance and Jupyter Notebook, and then run a sample notebook to connect to your local Databend, as well as write and visualize data within the notebook.

Before you start, make sure you have completed the following tasks:

- You have [Python](https://www.python.org/) installed on your system.
- Download the sample notebook [databend.ipynb](https://datafuse-1253727613.cos.ap-hongkong.myqcloud.com/integration/databend.ipynb) to a local folder.

### Step 1. Deploy Databend

1. Follow the [Deployment Guide](https://databend.rs/doc/deploy) to deploy a local Databend.
2. Create a SQL user in Databend. You will use this account to connect to Databend in Jupyter Notebook.

```sql
CREATE USER user1 IDENTIFIED BY 'abc123';
GRANT ALL ON *.* TO user1;
```

### Step 2. Deploy Jupyter Notebook

1. Install Jupyter Notebook with pip:

```shell
pip install notebook
```

2. Install dependencies with pip:

```shell
pip install sqlalchemy
pip install pandas
pip install pymysql
```

### Step 3. Run Sample Notebook

1. Run the command below to start Jupyter Notebook:

```shell
jupyter notebook
```

  This will start up Jupyter and your default browser should start (or open a new tab) to the following URL: http://localhost:8888/tree

![Alt text](../../../public/img/integration/notebook-tree.png)

2. On the **Files** tab, navigate to the sample notebook you downloaded and open it.

3. In the sample notebook, run the cells in order. By doing so, you create a table containing 5 rows in your local Databend, and visualize the data with a bar chart.

![Alt text](../../../public/img/integration/integration-gui-jupyter.png)